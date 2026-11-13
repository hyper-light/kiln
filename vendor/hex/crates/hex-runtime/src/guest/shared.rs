//! The one shared CPU VM consumes a real Index grant in the same Store/native
//! transaction as dedicated VMs. Its process receipt is not another resource ledger.

use std::path::Path;

use hex_api::{BackingGrant, BackingKey, BackingOffer, BackingPhase, Execution, LifecycleCause};
use hex_core::record::{NativeRecord, PoolRecord};
use sha2::{Digest, Sha256};

use super::pool::{self, Receipt, refused};
use super::selected::Phase;
use super::*;
use crate::devices::{self, Transaction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Binding {
    pub key: BackingKey,
    pub commitment: [u8; 32],
}

/// A transient view of initialized configuration, never inventory or a holder.
pub(super) struct GpuBacking {
    pub pool: [u8; 32],
    pub cost: hex_api::RuntimeCost,
    pub devices: Vec<hex_api::DeviceId>,
    pub physical: Vec<shards_api::PhysicalGpu>,
    pub path: PathBuf,
}

fn guest_route_key(
    class: Class,
    execution: Execution,
    gpu: hex_api::GpuEligibility,
) -> Result<hex_api::RouteKey, Refusal> {
    let key = hex_api::RouteKey {
        class,
        image: hex_api::ImagePlatform {
            os: hex_api::ImageOs::Linux,
            arch: route::host_platform()?.arch,
        },
        execution,
        gpu,
    };
    key.check()?;
    Ok(key)
}

fn backing_route_keys(
    execution: Execution,
    devices: &[hex_api::DeviceId],
) -> Result<(hex_api::RouteKey, Option<hex_api::RouteKey>), Refusal> {
    use hex_api::GpuEligibility;
    match execution {
        Execution::ShardsShared if devices.is_empty() => Ok((
            guest_route_key(Class::Oci, execution, GpuEligibility::None)?,
            None,
        )),
        Execution::ShardsDedicated if !devices.is_empty() => Ok((
            guest_route_key(Class::Oci, execution, GpuEligibility::Required)?,
            Some(guest_route_key(
                Class::Shards,
                execution,
                GpuEligibility::Required,
            )?),
        )),
        _ => Err(route::invalid(
            "Shards backing has no initialized route forms",
        )),
    }
}

impl GuestRuntime {
    /// Pure projection into the SAME owner's global bounded tuple union. This
    /// uses initialized forms and canonical rows, never a capacity/provider call
    /// or a receipt read while the owner's family publication fence is held.
    pub fn project_routes(
        &self,
        pools: &[PoolRecord],
        introduced: Option<(Execution, &[hex_api::DeviceId])>,
        projection: &mut devices::RouteProjection,
    ) -> Result<(), Refusal> {
        if pools.len() > crate::MAX_BOXES {
            return Err(Refusal::Full(crate::MAX_BOXES));
        }
        if !self.uses_books() || self.runtime_cost.is_none() {
            return Err(route::invalid(
                "route projection needs the initialized Shards owner/cost",
            ));
        }
        self.configured_allocation()?;
        projection.retain(
            guest_route_key(
                Class::Shards,
                Execution::ShardsDedicated,
                hex_api::GpuEligibility::None,
            )?,
            None,
        )?;
        for pool in pools {
            if !matches!(
                pool.execution,
                Execution::ShardsShared | Execution::ShardsDedicated
            ) {
                continue;
            }
            self.backing_configuration(pool)?;
            let (first, alias) = backing_route_keys(pool.execution, &pool.devices)?;
            projection.retain(first, Some(pool.key))?;
            if let Some(alias) = alias {
                projection.retain(alias, Some(pool.key))?;
            }
        }
        if let Some((execution, ids)) = introduced {
            match execution {
                Execution::ShardsShared if ids.is_empty() => {}
                Execution::ShardsDedicated => {
                    self.gpu_backing_definition_for(Some(ids))?.ok_or_else(|| {
                        route::invalid("new GPU backing has no initialized configuration")
                    })?;
                }
                Execution::ShardsShared => {
                    return Err(route::invalid("shared CPU backing cannot claim GPU IDs"));
                }
                Execution::Native | Execution::Firecracker => return Ok(()),
            }
            let (first, alias) = backing_route_keys(execution, ids)?;
            projection.introduce(first)?;
            if let Some(alias) = alias {
                projection.introduce(alias)?;
            }
        }
        Ok(())
    }
    /// Configure the real guest readiness workload after the exact measured VM
    /// cost and read-only shares. This verifies files only; it never queries GPU
    /// hardware, starts a Program/VM or creates a resource/holder record.
    pub fn with_gpu_observer(mut self, input: GpuObserverInput) -> Result<Self, Refusal> {
        self.gpu_observer = Some(GpuObserver::new(
            input,
            &self.config,
            self.configured_allocation()?,
        )?);
        Ok(self)
    }

    /// Persist a real Prepared Pool scope before Index admission. Configured IDs
    /// are claims only; this reads no GPU/provider and creates no VM or metadata.
    pub fn prepare_gpu_backing(&self) -> Result<Option<hex_api::BackingOffer>, Refusal> {
        self.prepare_gpu_family()
    }

    pub(super) fn gpu_backing_definition(&self) -> Result<Option<GpuBacking>, Refusal> {
        self.gpu_backing_definition_for(None)
    }

    /// A subset is supplied by retained canonical claims or the guarded
    /// preparation proposal. This only binds configuration; it selects no
    /// workload device and supplies neither observation nor resource authority.
    pub(super) fn gpu_backing_definition_for(
        &self,
        selected: Option<&[hex_api::DeviceId]>,
    ) -> Result<Option<GpuBacking>, Refusal> {
        if self.gpu_catalog.is_none() {
            return Ok(None);
        }
        let Some(observer) = self.gpu_observer.as_ref() else {
            // CPU preparation works without a Vulkan observer. No GPU route or
            // fabricated metadata is advertised for an uninitialized observer.
            return Ok(None);
        };
        if self.physical_entries()?.is_empty() {
            return Ok(None);
        }
        let allocation = self.configured_allocation()?;
        observer.check_allocation(allocation)?;
        self.gpu_backing_definition_under(selected, allocation.cost)
    }

    /// The GPU backing `selected` names under `cost`: the configured profile's, or a retained record's own (joint plan
    /// step 14: a backing recorded under the profile's census cost is recognized by its identity recomputed under
    /// that cost). No observer, catalog or physical entry: none.
    pub(super) fn gpu_backing_definition_under(
        &self,
        selected: Option<&[hex_api::DeviceId]>,
        cost: hex_api::RuntimeCost,
    ) -> Result<Option<GpuBacking>, Refusal> {
        let (Some(_), Some(observer)) = (self.gpu_catalog.as_ref(), self.gpu_observer.as_ref())
        else {
            return Ok(None);
        };
        let entries = self.physical_entries()?;
        if entries.is_empty() {
            return Ok(None);
        }
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(entries.len())
            .map_err(|_| Refusal::Full(hex_api::MAX_DEVICES))?;
        for entry in entries {
            devices.push(entry.info.id);
        }
        if devices
            .windows(2)
            .any(|pair| matches!(pair, [left,right] if left >= right))
        {
            return Err(route::invalid(
                "configured GPU backing claims are not unique",
            ));
        }
        let full = match selected {
            Some(selected) => {
                BackingOffer::check_selection_parts(&devices, selected)?;
                let full = selected == devices;
                // The checked subset cannot exceed this already fallibly
                // reserved full-family storage; extend cannot grow it.
                devices.clear();
                devices.extend_from_slice(selected);
                full
            }
            None => true,
        };
        observer.output_bytes(devices.len())?;
        let physical = self.catalog_physical(&devices)?;
        let pool = self.gpu_backing_pool(cost, &devices, &physical)?;
        let original = self.pool_path()?;
        // Full-set legacy receipts retain their exact path. Subsets use the
        // set-bound SHA-256 pool identity, never a reused Box/socket identity.
        let suffix = if full {
            ".gpu-observer"
        } else {
            ".gpu-observer."
        };
        let digits = if full {
            0
        } else {
            pool.len()
                .checked_mul(2)
                .ok_or_else(|| route::invalid("GPU backing pool hash length overflows"))?
        };
        let mut path = OsString::new();
        path.try_reserve_exact(
            original
                .as_os_str()
                .len()
                .checked_add(suffix.len())
                .and_then(|length| length.checked_add(digits))
                .ok_or_else(|| route::invalid("GPU backing receipt path length overflows"))?,
        )
        .map_err(|_| route::invalid("GPU backing receipt path storage is unavailable"))?;
        path.push(original.as_os_str());
        path.push(suffix);
        if !full {
            use std::fmt::Write as _;
            let mut encoded = String::new();
            encoded
                .try_reserve_exact(digits)
                .map_err(|_| route::invalid("GPU backing pool hash storage is unavailable"))?;
            for byte in pool {
                write!(&mut encoded, "{byte:02x}")
                    .map_err(|_| route::invalid("GPU backing pool hash encoding failed"))?;
            }
            path.push(encoded);
        }
        Ok(Some(GpuBacking {
            pool,
            cost,
            devices,
            physical,
            path: PathBuf::from(path),
        }))
    }
    /// The existing v2 identity formula also names an empty drain proposal.
    /// Empty is never a Prepared GPU record, route or physical selector grant.
    pub(super) fn gpu_backing_pool(
        &self,
        cost: hex_api::RuntimeCost,
        devices: &[hex_api::DeviceId],
        physical: &[shards_api::PhysicalGpu],
    ) -> Result<[u8; 32], Refusal> {
        if devices.len() != physical.len() || devices.len() > hex_api::MAX_DEVICES {
            return Err(route::invalid(
                "GPU pool identity has inconsistent bounded selectors",
            ));
        }
        let observer = self
            .gpu_observer
            .as_ref()
            .ok_or_else(|| route::invalid("GPU identity has no configured observer"))?;
        let mut digest = Sha256::new();
        digest.update(b"hex-shards-gpu-observer/v2");
        digest.update(self.pool_configuration(cost)?);
        digest.update(observer.identity());
        for (device, selector) in devices.iter().zip(physical) {
            digest.update(device.key.to_be_bytes());
            let argument = shards_api::physical::argument(*selector).map_err(|why| {
                refusal(
                    "encoding configured GPU backing selector",
                    &shards_api::Refusal::Gpu(why),
                )
            })?;
            digest.update(
                u64::try_from(argument.len())
                    .map_err(|_| route::invalid("GPU backing selector length overflows"))?
                    .to_be_bytes(),
            );
            digest.update(argument.as_encoded_bytes());
        }
        Ok(digest.finalize().into())
    }

    /// Read-only initialized-backend and retained VM-receipt validation during
    /// the SAME owner's whole-record preflight. The caller joins every canonical
    /// backing/holder first; this never probes, launches, cleans up or writes.
    pub fn preflight_box(&self, record: &hex_core::record::BoxRecord) -> Result<(), Refusal> {
        // A retired box is kept for its owner's delete, and a VM recorded under the census cost is its node start's to
        // retire (joint plan step 14): neither meets the exact checks below.
        if record.retired || self.census_vm(record)? {
            return Ok(());
        }
        let spec = &record.spec;
        route::guest_spec(spec)?;
        match spec.route_execution() {
            Some(Execution::ShardsShared) => {
                let key = spec
                    .route
                    .and_then(|receipt| receipt.backing)
                    .ok_or(super::books::uncertain(&spec.id)?)?;
                // A box on a backing its node recorded under the census cost goes with that backing's retirement.
                if let Some(backing) = self
                    .authority()?
                    .pool_records()?
                    .into_iter()
                    .find(|backing| backing.key == key)
                    && self.census_backing(&backing)?
                {
                    return Ok(());
                }
                let cost = self
                    .runtime_cost
                    .ok_or(super::books::uncertain(&spec.id)?)?;
                if spec.class.class() != Class::Oci
                    || !spec.devices.is_empty()
                    || spec.allocation.is_some()
                    || key.pool != self.pool_configuration(cost)?
                    || self.shared_key()?.is_some_and(|bound| bound != key)
                {
                    return Err(super::books::uncertain(&spec.id)?);
                }
                Ok(())
            }
            Some(Execution::ShardsDedicated) => {
                self.check_allocation(spec)?;
                selected::Record::intent(record, &self.config)?;
                if !spec.devices.is_empty() {
                    // Catalog correspondence is configuration, not a fresh GPU
                    // observation. The full native/report join is still required.
                    self.catalog_physical(&spec.devices)?;
                }
                self.preflight_selected_receipt(record)
            }
            // Only an already charged exact dedicated legacy record is
            // unambiguous here. A legacy shared VM cannot acquire a new grant.
            None if spec.class.class() == Class::Shards || !spec.devices.is_empty() => {
                self.check_allocation(spec)?;
                selected::Record::intent(record, &self.config)?;
                if !spec.devices.is_empty() {
                    self.catalog_physical(&spec.devices)?;
                }
                self.preflight_selected_receipt(record)
            }
            Some(_) | None => Err(super::books::uncertain(&spec.id)?),
        }
    }

    fn preflight_selected_receipt(
        &self,
        record: &hex_core::record::BoxRecord,
    ) -> Result<(), Refusal> {
        let id = &record.spec.id;
        let receipt = match selected::read(&self.selected_path(id)?)? {
            Some(receipt) => receipt,
            None if record.phase == hex_core::record::Phase::Creating && record.init.is_none() => {
                return Ok(());
            }
            None => return Err(super::books::uncertain(id)?),
        };
        receipt.matches_record(record)?;
        if !receipt.devices.is_empty()
            && self.catalog_physical(&receipt.devices)? != receipt.physical
        {
            return Err(super::books::uncertain(id)?);
        }
        match &receipt.phase {
            Phase::Running(identity)
            | Phase::Stopping(identity)
            | Phase::Released {
                identity: Some(identity),
                ..
            } => managed::same_init(record, identity)?,
            Phase::Released { identity: None, .. } if record.init.is_some() => {
                return Err(super::books::uncertain(id)?);
            }
            Phase::Intent | Phase::Released { identity: None, .. } => {}
        }
        if receipt.report.is_some() {
            receipt.checked_report()?;
        }
        Ok(())
    }

    /// Read-only route/profile validation for the SAME owner's retained-record
    /// preflight. This performs no GPU query, file cleanup, launch or adoption.
    pub(super) fn backing_configuration(
        &self,
        record: &PoolRecord,
    ) -> Result<Option<GpuBacking>, Refusal> {
        record.check()?;
        let key = record.key;
        if self.runtime_cost != Some(record.cost) {
            return Err(refused(key, LifecycleCause::Uncertain));
        }
        let gpu = match record.execution {
            Execution::ShardsShared
                if record.devices.is_empty()
                    && self.shared_key()?.is_none_or(|bound| bound == key)
                    && key.pool == self.pool_configuration(record.cost)? =>
            {
                None
            }
            Execution::ShardsDedicated => Some(self.gpu_definition_for_record(record)?),
            _ => return Err(refused(key, LifecycleCause::Uncertain)),
        };
        let allocation = record
            .cost
            .allocate(hex_api::CapacityResources::default())?;
        let guest = allocation.guest()?;
        let configured = selected::resources(&self.config)?;
        if record
            .allocation
            .is_some_and(|retained| retained != allocation)
            || guest.cpu_milli != configured.cpu_milli
            || guest.mem_bytes != configured.mem_bytes
        {
            return Err(refused(key, LifecycleCause::Uncertain));
        }
        Ok(gpu)
    }

    /// Whether pool `record` is this runtime's shared backing as its node recorded it under its profile's census cost
    /// (joint plan step 14): its cost the profile's but for its census numbers, and its identity this runtime's
    /// configuration recomputed under that cost. It is retired ([`Self::retire_census`]) at its node's start, so the
    /// preflight passes it, and no exact check sees it.
    pub(super) fn census_backing(&self, record: &PoolRecord) -> Result<bool, Refusal> {
        let Some(current) = self.runtime_cost else {
            return Ok(false);
        };
        Ok(record.execution == Execution::ShardsShared
            && record.devices.is_empty()
            && record.cost != current
            && record.cost.census_free() == current
            && record.key.pool == self.pool_configuration(record.cost)?)
    }

    pub fn preflight_backing(&self, record: &PoolRecord) -> Result<(), Refusal> {
        if self.census_backing(record)? || self.census_gpu_backing(record)?.is_some() {
            return Ok(());
        }
        let gpu = self.backing_configuration(record)?;
        let (path, physical): (&Path, &[shards_api::PhysicalGpu]) = match &gpu {
            Some(definition) => (definition.path.as_path(), &definition.physical),
            None => (self.pool_path()?, &[]),
        };
        self.preflight_pool_receipt_at(record, path, physical)?;
        Ok(())
    }

    fn preflight_pool_receipt_at(
        &self,
        record: &PoolRecord,
        path: &Path,
        physical: &[shards_api::PhysicalGpu],
    ) -> Result<(), Refusal> {
        let receipt = match pool::read(path)? {
            Some(receipt) => receipt,
            None if record.phase == BackingPhase::Prepared => return Ok(()),
            None if record.phase == BackingPhase::Granted && record.init.is_none() => return Ok(()),
            // prepare_backing removes a positively completed local receipt before
            // issuing a new generation; the canonical terminal row is retained.
            None if record.phase == BackingPhase::Released && record.exit.is_some() => {
                return Ok(());
            }
            // Cancellation before any grant is cleanup-readable only. The SAME
            // native owner must recheck no-submission before later Index-ACK
            // metadata removal; this branch supplies no usable Guest/end fact.
            None if record.phase == BackingPhase::Released
                && record.allocation.is_none()
                && record.init.is_none()
                && record.exit.is_none() =>
            {
                return Ok(());
            }
            None => return Err(refused(record.key, LifecycleCause::Uncertain)),
        };
        receipt.matches(record)?;
        if receipt.physical != physical {
            return Err(refused(record.key, LifecycleCause::Uncertain));
        }
        if let Some(readiness) = &receipt.readiness {
            let observer = self
                .gpu_observer
                .as_ref()
                .ok_or_else(|| refused(record.key, LifecycleCause::Uncertain))?;
            if readiness.configuration != observer.identity()
                || readiness.max_queues != observer.queues()
            {
                return Err(refused(record.key, LifecycleCause::Uncertain));
            }
        }
        if let Some(attempt) = &receipt.attempt {
            let observer = self
                .gpu_observer
                .as_ref()
                .ok_or_else(|| refused(record.key, LifecycleCause::Uncertain))?;
            attempt.check_config(observer, physical.len())?;
        }
        match &receipt.phase {
            Phase::Running(identity)
            | Phase::Stopping(identity)
            | Phase::Released {
                identity: Some(identity),
                ..
            } => managed::same_native(NativeRecord::Pool(record), identity)?,
            // A local controller publication can be absent after durable
            // Released. Metadata preflight cannot turn that into native
            // absence or block cleanup(None) against this exact canonical
            // scope/commitment. No usable identity is minted here.
            Phase::Intent | Phase::Released { identity: None, .. } => {}
        }
        Ok(())
    }

    pub(super) fn managed_shared(&self) -> bool {
        self.uses_books() || self.runtime_cost.is_some()
    }

    fn pool_path(&self) -> Result<&Path, Refusal> {
        self.record.as_deref().ok_or_else(|| {
            route::invalid("managed shared backing needs its durable guest receipt path")
        })
    }

    /// Existing single generation binding, copied under a short lock. It is
    /// invalidated only by the explicit Index ACK after canonical removal.
    pub(super) fn shared_key(&self) -> Result<Option<BackingKey>, Refusal> {
        self.shared_pool
            .lock()
            .map(|bound| *bound)
            .map_err(|_| route::invalid("shared backing generation lock is poisoned"))
    }

    fn bind_pool(&self, key: BackingKey) -> Result<(), Refusal> {
        let mut bound = self
            .shared_pool
            .lock()
            .map_err(|_| refused(key, LifecycleCause::Uncertain))?;
        if bound.is_some_and(|known| known != key) {
            return Err(refused(key, LifecycleCause::Uncertain));
        }
        *bound = Some(key);
        Ok(())
    }

    fn clear_shared_key(&self, key: BackingKey) -> Result<(), Refusal> {
        let mut bound = self
            .shared_pool
            .lock()
            .map_err(|_| refused(key, LifecycleCause::Uncertain))?;
        if *bound != Some(key) {
            return Err(refused(key, LifecycleCause::Uncertain));
        }
        *bound = None;
        Ok(())
    }

    /// Called after registration of the SAME Books owner, before capacity/recovery.
    /// Store persists Prepared/scope before Index may issue the grant. No VM starts.
    pub fn prepare_backing(&self) -> Result<BackingOffer, Refusal> {
        let authority = self.authority()?;
        if let Some(key) = self.shared_key()? {
            return self.pool_record(key)?.offer();
        }
        let path = self.pool_path()?;
        if let Some(receipt) = pool::read(path)? {
            let key = receipt.original.key;
            let record = self.pool_record(key)?;
            receipt.matches(&record)?;
            self.preflight_backing(&record)?;
            if record.phase == BackingPhase::Released {
                if !matches!(receipt.phase, Phase::Released { .. }) {
                    return Err(refused(key, LifecycleCause::Uncertain));
                }
                let books = self
                    .books
                    .get()
                    .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
                devices::with_pool_transaction(books, &key, &[], |transaction| {
                    let current = transaction
                        .read_pool(&key)?
                        .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
                    receipt.matches(&current)?;
                    if current.phase != BackingPhase::Released {
                        return Err(refused(key, LifecycleCause::Uncertain));
                    }
                    remove_receipt(path)
                })?;
            } else {
                if record.execution != Execution::ShardsShared || !record.devices.is_empty() {
                    return Err(refused(key, LifecycleCause::Uncertain));
                }
                self.bind_pool(key)?;
                return record.offer();
            }
        }
        let allocation = self.configured_allocation()?;
        let pool = self.pool_configuration(allocation.cost)?;
        let snapshot = authority.pool_records()?;
        let expected = snapshot.iter().find(|row| row.key.pool == pool);
        let mut validate = |records: &devices::Records, pools: &[PoolRecord]| {
            let mut current = None;
            for row in pools {
                if row.execution != Execution::ShardsShared {
                    continue;
                }
                self.backing_configuration(row)?;
                if current.replace(row).is_some() {
                    return Err(refused(row.key, LifecycleCause::Uncertain));
                }
            }
            if current != expected {
                return Err(route::invalid(
                    "shared CPU backing changed before guarded publication",
                ));
            }
            for (id, boxed) in records {
                let boxed = match boxed {
                    Ok(boxed) => boxed,
                    Err(_) => return Err(super::books::uncertain(id)?),
                };
                // A retired box's backing went with its retirement: it names no backing to agree with.
                if !boxed.retired
                    && boxed.spec.route_execution() == Some(Execution::ShardsShared)
                    && boxed.spec.route.and_then(|route| route.backing)
                        != current.map(|row| row.key)
                {
                    return Err(super::books::uncertain(id)?);
                }
            }
            Ok(())
        };
        let row = authority.prepare_pool(
            pool,
            Execution::ShardsShared,
            allocation.cost,
            &[],
            (&pool, &[], expected),
            &mut validate,
        )?;
        if row.key.pool != pool || row.cost != allocation.cost || !row.devices.is_empty() {
            return Err(refused(row.key, LifecycleCause::Uncertain));
        }
        self.preflight_backing(&row)?;
        self.bind_pool(row.key)?;
        row.offer()
    }

    fn pool_configuration(&self, cost: hex_api::RuntimeCost) -> Result<[u8; 32], Refusal> {
        if self.config.shares.len() > shares::MAX_SHARES {
            return Err(route::invalid(
                "shared backing exceeds the configured share bound",
            ));
        }
        let paths = self.native_paths.as_ref().ok_or_else(|| {
            route::invalid("shared backing needs the configured native/probe path pair")
        })?;
        let mut hash = Sha256::new();
        let mut part = |bytes: &[u8]| -> Result<(), Refusal> {
            hash.update(
                u64::try_from(bytes.len())
                    .map_err(|_| route::invalid("backing configuration length overflows"))?
                    .to_be_bytes(),
            );
            hash.update(bytes);
            Ok(())
        };
        // Versioned domain separation is protocol identity, not a resource allowance.
        part(b"hex-shards-shared-backing/v1")?;
        part(&cost.id)?;
        for resources in [
            cost.floor,
            cost.ceiling,
            cost.quantum,
            cost.guest_overhead,
            cost.overhead,
        ] {
            for value in [
                resources.cpu_milli,
                resources.mem_bytes,
                resources.disk_bytes,
            ] {
                part(&value.to_be_bytes())?;
            }
        }
        for path in [
            &self.config.binary,
            &self.config.kernel,
            &self.config.initrd,
        ] {
            part(path.as_os_str().as_encoded_bytes())?;
        }
        part(paths.cache().as_os_str().as_encoded_bytes())?;
        part(paths.home().as_os_str().as_encoded_bytes())?;
        part(&self.config.vcpus.to_be_bytes())?;
        part(&self.config.memory_mib.to_be_bytes())?;
        part(self.config.staged.as_bytes())?;
        for share in &self.config.shares {
            part(share.dir.as_os_str().as_encoded_bytes())?;
            part(share.at.as_bytes())?;
            part(&[u8::from(share.read_only)])?;
        }
        Ok(hash.finalize().into())
    }

    /// Retires what this backend's node recorded under its profile's census cost (joint plan step 14; ROOT,
    /// 2026-11-13: no legacy receipt runs on with a census number as its bound), run at its node's start once its
    /// natives are up, before any preparation or recovery: each shared backing and the boxes on it
    /// ([`Self::retire_census_backings`]), each VM box ([`Self::retire_census_vms`]) and each dedicated GPU backing
    /// (`retire_census_gpu_backings`). The preflight passes each of them for this, and no exact check sees them.
    pub fn retire_census(&self) -> Result<(), Refusal> {
        self.retire_census_backings()?;
        // VM boxes before the GPU backings they consume, so each backing goes with nothing left on it.
        self.retire_census_vms()?;
        self.retire_census_gpu_backings()
    }

    /// Whether VM box `record` is one this runtime owns whose receipt is its profile's under its census numbers (joint
    /// plan step 14): its receipt's cost the profile's but for them, and the envelope it admits this runtime's
    /// configured one. Retired at its node's start ([`Self::retire_census`]), so the preflight passes it.
    pub(super) fn census_vm(&self, record: &hex_core::record::BoxRecord) -> Result<bool, Refusal> {
        let (Some(current), Some(allocation)) = (self.runtime_cost, record.spec.allocation) else {
            return Ok(false);
        };
        if record.retired
            || !self.owns_record(&record.spec)
            || allocation.cost == current
            || allocation.cost.census_free() != current
        {
            return Ok(false);
        }
        let guest = allocation.guest()?;
        let configured = self.dedicated_resources()?;
        Ok(guest.cpu_milli == configured.cpu_milli && guest.mem_bytes == configured.mem_bytes)
    }

    /// Retires each VM box this runtime owns whose receipt carries its profile's census numbers ([`Self::census_vm`]):
    /// its VM stopped through its receipt as its delete stops it and that receipt finalized and forgotten, and the box
    /// kept retired (`BoxRecord::retire`).
    fn retire_census_vms(&self) -> Result<(), Refusal> {
        let books = self
            .books
            .get()
            .ok_or_else(|| Refusal::Broken("guest has no initialized record authority".into()))?;
        for (id, record) in self.authority()?.records()? {
            let Ok(record) = record else {
                continue;
            };
            if !self.census_vm(&record)? {
                continue;
            }
            if matches!(record.spec.class, hex_api::ClassSpec::ShardsImage(_)) {
                self.images.retire_image(record)?;
                continue;
            }
            let keys = self.turn_keys(&record.spec.devices)?;
            let launched = selected::read(&self.selected_path(&id)?)?.is_some();
            devices::with_transaction(books, &id, &keys, |transaction| {
                let mut current = transaction
                    .read(&id)?
                    .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
                if launched {
                    self.delete_selected(&id, transaction)?;
                    self.finalize_selected(&id, transaction)?;
                }
                current.retire();
                transaction.put(&current)
            })?;
            if launched {
                self.forget_selected(&id)?;
            }
        }
        Ok(())
    }

    /// Retires each shared backing its node recorded under its profile's census cost (joint plan step 14; ROOT,
    /// 2026-11-13, by decision 9's rule: a census row is evidence alone, so the profile's identity changed with its
    /// census-free cost, `RuntimeCost::census_free`), never leaking it: in its pool turn, each box on it is kept, ready,
    /// its end unknown and marked retired, so every call but its owner's delete is refused `Retired`; its scope is stopped
    /// and released through its receipt as a release's is; and its record is removed. A backing of another profile is
    /// none of this one's, left to the checks that refuse it.
    fn retire_census_backings(&self) -> Result<(), Refusal> {
        for record in self.authority()?.pool_records()? {
            if !self.census_backing(&record)? {
                continue;
            }
            let key = record.key;
            let books = self
                .books
                .get()
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            devices::with_pool_transaction(books, &key, &[], |transaction| {
                let mut record = transaction
                    .read_pool(&key)?
                    .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
                for boxed in transaction.records()?.into_values() {
                    let mut boxed = boxed.map_err(|_| refused(key, LifecycleCause::Uncertain))?;
                    if boxed.retired
                        || boxed.spec.route.and_then(|route| route.backing) != Some(key)
                    {
                        continue;
                    }
                    boxed.retire();
                    transaction.put(&boxed)?;
                }
                if record.phase != BackingPhase::Released {
                    self.stop_and_release(transaction, &mut record, &|_| Ok(()))?;
                }
                transaction.remove_pool(&key)
            })?;
        }
        Ok(())
    }

    pub(super) fn pool_record(&self, key: BackingKey) -> Result<PoolRecord, Refusal> {
        self.authority()?
            .pool_records()?
            .into_iter()
            .find(|record| record.key == key)
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))
    }

    /// Recovery consumes the original already-admitted grant. It never invents
    /// a new generation or restarts an interrupted native launch. A node whose
    /// guest serves its dedicated GPU pools alone (a Linux node's, whose OCI
    /// boxes run natively) prepares no shared backing and has no shared scope
    /// to recover; a retained shared record without its prepared binding is
    /// still refused.
    pub fn recover_backing(&self) -> Result<(), Refusal> {
        self.recover_gpu_backing()?;
        let Some(key) = self.shared_key()? else {
            if self
                .authority()?
                .pool_records()?
                .iter()
                .any(|record| record.execution == Execution::ShardsShared)
            {
                return Err(route::invalid("prepare the shared backing before recovery"));
            }
            return Ok(());
        };
        let mut record = self.pool_record(key)?;
        self.preflight_backing(&record)?;
        if matches!(
            record.phase,
            BackingPhase::Granted | BackingPhase::Live | BackingPhase::Draining
        ) && self.observe_ended_pool(&mut record)?
        {
            // Keep the native grant while ordinary recovery removes each inner
            // record using that same positive parent-scope absence witness.
            return Ok(());
        }
        match (record.phase, record.allocation) {
            (BackingPhase::Prepared | BackingPhase::Released, _) => Ok(()),
            (BackingPhase::Granted | BackingPhase::Live, Some(allocation)) => {
                if !record.devices.is_empty() {
                    return Err(refused(key, LifecycleCause::Uncertain));
                }
                self.initialize_pool(BackingGrant {
                    key,
                    allocation,
                    devices: Vec::new(),
                })?;
                Ok(())
            }
            _ => Err(refused(key, LifecycleCause::Uncertain)),
        }
    }

    fn observe_ended_pool(&self, record: &mut PoolRecord) -> Result<bool, Refusal> {
        let key = record.key;
        let books = self
            .books
            .get()
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        devices::with_pool_transaction(books, &key, &[], |transaction| {
            let mut current = transaction
                .read_pool(&key)?
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            if current.native_commitment()? != record.native_commitment()? {
                return Err(refused(key, LifecycleCause::Uncertain));
            }
            let path = self.pool_path()?;
            let Some(mut receipt) = pool::read(path)? else {
                return Ok(false);
            };
            receipt.matches(&current)?;
            let identity = match &receipt.phase {
                Phase::Running(identity)
                | Phase::Stopping(identity)
                | Phase::Released {
                    identity: Some(identity),
                    ..
                } => {
                    managed::same_native(NativeRecord::Pool(&current), identity)?;
                    Some(identity)
                }
                Phase::Intent | Phase::Released { identity: None, .. }
                    if current.init.is_none() =>
                {
                    None
                }
                _ => return Err(refused(key, LifecycleCause::Uncertain)),
            };
            let native = transaction.attach_managed(identity)?;
            let terminal = native
                .terminal()
                .map_err(|why| refusal("observing shared backing during recovery", &why))?;
            // AC-T7.4.1: its controller, the VM, ended while no node ran, so there is no guest to take back or
            // dial. A member of its scope that outlived it is ended now, as the pool's release ends one and
            // within the same learned bound, so the end recorded is the whole scope's, the witness inner
            // recovery takes (gpu_pool's recover_gpu_terminal ends one so too).
            let end = match terminal {
                None => return Ok(false),
                Some(end) if end.scope == shards_api::TerminalScope::Released => end,
                Some(_) => {
                    let identity = identity.map(copy_identity).transpose()?;
                    if let Some(identity) = &identity {
                        receipt.phase = Phase::Stopping(copy_identity(identity)?);
                        pool::write(path, &receipt)?;
                    }
                    managed::stop_scope(
                        transaction,
                        identity.as_ref(),
                        self.first_dial_bound()?,
                        refused(key, LifecycleCause::Uncertain),
                    )?
                }
            };
            current.phase = BackingPhase::Draining;
            current.exit = Some(exit_of(end.status));
            transaction.put_pool(&current)?;
            *record = current;
            Ok(true)
        })
    }

    pub(super) fn initialize_pool(&self, grant: BackingGrant) -> Result<BackingOffer, Refusal> {
        grant.check()?;
        let key = grant.key;
        if self.shared_key()? != Some(key) {
            return Err(refused(key, LifecycleCause::Pending));
        }
        let budget = managed::Budget::new(self.first_dial_bound()?)?;
        let books = self
            .books
            .get()
            .ok_or_else(|| refused(key, LifecycleCause::Pending))?;
        devices::with_pool_transaction(books, &key, &[], |transaction| {
            let record = transaction.grant_pool(&grant)?;
            self.preflight_backing(&record)?;
            if record.execution != Execution::ShardsShared || !record.devices.is_empty() {
                return Err(refused(key, LifecycleCause::Uncertain));
            }
            let booted = match self.running()? {
                Some(booted) => {
                    self.matches_pool(&booted, &record)?;
                    self.pool_alive(&booted, key)?;
                    booted
                }
                None => self.boot_pool(transaction, record, budget)?,
            };
            let mut record = transaction
                .read_pool(&key)?
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            self.matches_pool(&booted, &record)?;
            if record.phase == BackingPhase::Granted {
                record.phase = BackingPhase::Live;
                transaction.put_pool(&record)?;
            }
            if record.phase != BackingPhase::Live {
                return Err(refused(key, LifecycleCause::Uncertain));
            }
            let offer = record.offer()?;
            let mut state = self.lock()?;
            if state
                .as_ref()
                .is_some_and(|known| known.backing != booted.backing)
            {
                return Err(refused(key, LifecycleCause::Uncertain));
            }
            *state = Some(booted);
            Ok(offer)
        })
    }

    fn boot_pool(
        &self,
        transaction: &dyn Transaction,
        record: PoolRecord,
        budget: managed::Budget,
    ) -> Result<triomphe::Arc<Booted>, Refusal> {
        let path = self.pool_path()?;
        let key = record.key;
        let mut receipt = match pool::read(path)? {
            Some(receipt) => {
                receipt.matches(&record)?;
                receipt
            }
            None if record.phase == BackingPhase::Granted && record.init.is_none() => {
                if record.allocation != Some(self.configured_allocation()?) {
                    return Err(refused(key, LifecycleCause::Uncertain));
                }
                Receipt::intent(record, Vec::new(), &self.config)?
            }
            None => return Err(refused(key, LifecycleCause::Uncertain)),
        };
        let binding = Binding {
            key,
            commitment: receipt.original.native_commitment()?,
        };
        let spec = self.pool_spec(&receipt)?;
        let socket = self.pool_socket(key)?;
        let plan = self.managed_plan(&spec, &socket, budget)?;
        let guest = match &receipt.phase {
            Phase::Running(identity) => {
                receipt.checked_report()?;
                let canonical = transaction
                    .read_pool(&key)?
                    .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
                managed::same_native(NativeRecord::Pool(&canonical), identity)?;
                if identity.vsock != socket {
                    return Err(refused(key, LifecycleCause::Uncertain));
                }
                managed::attach(transaction, plan, identity, budget)?
            }
            Phase::Intent => {
                // An existing Intent is an interrupted attempt, not permission
                // to spawn another controller or discard uncertain helpers.
                if pool::read(path)?.is_some() {
                    return Err(refused(key, LifecycleCause::Uncertain));
                }
                budget.remaining()?;
                pool::write(path, &receipt)?;
                managed::boot(transaction, plan, budget)?
            }
            Phase::Stopping(_) | Phase::Released { .. } => {
                return Err(refused(key, LifecycleCause::Uncertain));
            }
        };
        let identity = shards_api::Guest::identity(&guest)
            .map_err(|why| refusal("reading managed shared guest identity", &why))?;
        let canonical = transaction
            .read_pool(&key)?
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        receipt.matches(&canonical)?;
        managed::same_native(NativeRecord::Pool(&canonical), &identity)?;
        if identity.vsock != socket {
            return Err(refused(key, LifecycleCause::Uncertain));
        }
        receipt.checked_report()?;
        receipt.phase = Phase::Running(identity);
        pool::write(path, &receipt)?;
        let booted = self.connect_managed(guest, None, Some(binding), budget)?;
        let guest_capacity = booted
            .client
            .capacity(Class::Oci)?
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        guest_capacity.check()?;
        let allocation = receipt
            .original
            .allocation
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        let room = allocation
            .guest()?
            .checked_sub(allocation.cost.guest_overhead)
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        if !guest_capacity.total.holds(room) {
            return Err(refused(key, LifecycleCause::Uncertain));
        }
        budget.remaining()?;
        Ok(booted)
    }

    pub(super) fn pool_spec(&self, receipt: &Receipt) -> Result<shards_api::GuestSpec, Refusal> {
        receipt.check()?;
        let guest = receipt
            .original
            .allocation
            .ok_or_else(|| refused(receipt.original.key, LifecycleCause::Uncertain))?
            .guest()?;
        if guest.cpu_milli.checked_rem(1000) != Some(0)
            || guest.mem_bytes.checked_rem(1 << 20) != Some(0)
        {
            return Err(refused(receipt.original.key, LifecycleCause::Uncertain));
        }
        let mut spec = self.cpu_spec();
        spec.vcpus = u32::try_from(
            guest
                .cpu_milli
                .checked_div(1000)
                .ok_or_else(|| refused(receipt.original.key, LifecycleCause::Uncertain))?,
        )
        .map_err(|_| refused(receipt.original.key, LifecycleCause::Uncertain))?;
        spec.memory_mib = guest
            .mem_bytes
            .checked_div(1 << 20)
            .ok_or_else(|| refused(receipt.original.key, LifecycleCause::Uncertain))?;
        Ok(spec)
    }

    pub(super) fn pool_socket(&self, key: BackingKey) -> Result<PathBuf, Refusal> {
        use std::fmt::Write as _;
        let parent = self
            .pool_path()?
            .parent()
            .ok_or_else(|| route::invalid("shared backing receipt has no parent"))?;
        let mut path = super::dedicated::copy_path(parent)?;
        let mut name = String::new();
        let digits = key
            .generation
            .len()
            .checked_mul(2)
            .ok_or_else(|| route::invalid("pool scope name overflows"))?;
        let length = "hex-pool-"
            .len()
            .checked_add(digits)
            .and_then(|length| length.checked_add(".sock".len()))
            .ok_or_else(|| route::invalid("pool socket name overflows"))?;
        name.try_reserve_exact(length)
            .map_err(|_| Refusal::Exhausted {
                what: "shared guest socket name",
                bound: length,
            })?;
        name.push_str("hex-pool-");
        for byte in key.generation {
            write!(&mut name, "{byte:02x}")
                .map_err(|_| route::invalid("pool scope cannot be formatted"))?;
        }
        name.push_str(".sock");
        path.try_reserve_exact(
            length
                .checked_add(1)
                .ok_or_else(|| route::invalid("pool socket path overflows"))?,
        )
        .map_err(|_| Refusal::Exhausted {
            what: "shared guest socket path",
            bound: length,
        })?;
        path.push(name);
        Ok(path)
    }

    pub(super) fn matches_pool(&self, booted: &Booted, record: &PoolRecord) -> Result<(), Refusal> {
        if booted.backing
            != Some(Binding {
                key: record.key,
                commitment: record.native_commitment()?,
            })
        {
            return Err(refused(record.key, LifecycleCause::Uncertain));
        }
        Ok(())
    }

    pub(super) fn pool_alive(&self, booted: &Booted, key: BackingKey) -> Result<(), Refusal> {
        let exited = booted
            .guest
            .process_exited()
            .map_err(|why| refusal("observing shared controller", &why))?;
        let scope = booted
            .guest
            .terminal_scope()
            .map_err(|why| refusal("observing shared native scope", &why))?;
        if exited || scope.is_some() {
            return Err(refused(key, LifecycleCause::Uncertain));
        }
        Ok(())
    }

    /// The shared backing `key`'s guest overhead, measured at this capacity answer where its guest runs (joint plan
    /// step 14, C7; [`guest_seen`]), kept per backing generation: a new generation starts cold. A read that fails, or a
    /// guest not running, is no sample: what was seen stays. No lock is held over the guest's answers.
    fn measured_guest(&self, key: BackingKey) -> Result<hex_api::LearnedOverhead, Refusal> {
        let kept = match *self
            .guest_overhead
            .lock()
            .map_err(|_| refused(key, LifecycleCause::Uncertain))?
        {
            Some((seen, overhead)) if seen == key => overhead,
            _ => hex_api::LearnedOverhead::default(),
        };
        let measured = self
            .shared_running()
            .ok()
            .and_then(|booted| {
                let guest = booted.client.capacity(Class::Oci).ok()??;
                let records = self.authority().ok()?.records().ok()?;
                let mut boxes = 0u64;
                for record in records.values().flatten() {
                    if record.spec.route.and_then(|route| route.backing) == Some(key) {
                        boxes = boxes.saturating_add(
                            booted.client.usage(&record.spec.id).ok()?.memory_bytes,
                        );
                    }
                }
                Some(guest_seen(kept, &guest, boxes))
            })
            .unwrap_or(kept);
        let mut seen = self
            .guest_overhead
            .lock()
            .map_err(|_| refused(key, LifecycleCause::Uncertain))?;
        // Another answer may have measured meanwhile: the most of the two.
        let merged = match *seen {
            Some((at, other)) if at == key => hex_api::LearnedOverhead {
                mem_bytes: measured.mem_bytes.max(other.mem_bytes),
                ..measured
            },
            _ => measured,
        };
        *seen = Some((key, merged));
        Ok(merged)
    }

    pub(super) fn shared_running(&self) -> Result<triomphe::Arc<Booted>, Refusal> {
        let key = self
            .shared_key()?
            .ok_or_else(|| route::invalid("shared backing has not been prepared and granted"))?;
        let record = self.pool_record(key)?;
        if record.phase != BackingPhase::Live {
            return Err(refused(key, LifecycleCause::Pending));
        }
        let booted = self
            .running()?
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        self.matches_pool(&booted, &record)?;
        self.pool_alive(&booted, key)?;
        Ok(booted)
    }

    /// A retained canonical parent scope, rather than an absent client/cache or
    /// controller PID, proves an inner payload gone after an unexpected VM end.
    pub(super) fn shared_for_record(
        &self,
        boxed: &hex_core::record::BoxRecord,
    ) -> Result<Option<triomphe::Arc<Booted>>, Refusal> {
        let key = boxed
            .spec
            .route
            .filter(|route| route.key.execution == Execution::ShardsShared)
            .and_then(|route| route.backing)
            .ok_or(super::books::uncertain(&boxed.spec.id)?)?;
        if boxed.retired {
            return Err(boxed.retired_refusal()?);
        }
        if self.shared_key()? != Some(key) {
            return Err(super::books::uncertain(&boxed.spec.id)?);
        }
        let record = self.pool_record(key)?;
        record.check()?;
        if record.execution != Execution::ShardsShared || !record.devices.is_empty() {
            return Err(refused(key, LifecycleCause::Uncertain));
        }
        if record.phase == BackingPhase::Released {
            return Ok(None);
        }
        if let Some(booted) = self.running()? {
            self.matches_pool(&booted, &record)?;
            let scope = booted
                .guest
                .terminal_scope()
                .map_err(|why| refusal("observing original shared parent scope", &why))?;
            if scope == Some(shards_api::TerminalScope::Released) {
                return Ok(None);
            }
            if record.phase != BackingPhase::Live {
                return Err(refused(key, LifecycleCause::Pending));
            }
            self.pool_alive(&booted, key)?;
            return Ok(Some(booted));
        }
        let books = self
            .books
            .get()
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        devices::with_pool_transaction(books, &key, &[], |transaction| {
            let current = transaction
                .read_pool(&key)?
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            if current.native_commitment()? != record.native_commitment()? {
                return Err(refused(key, LifecycleCause::Uncertain));
            }
            let receipt = pool::read(self.pool_path()?)?
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            receipt.matches(&current)?;
            let identity = match &receipt.phase {
                Phase::Running(identity)
                | Phase::Stopping(identity)
                | Phase::Released {
                    identity: Some(identity),
                    ..
                } => {
                    managed::same_native(NativeRecord::Pool(&current), identity)?;
                    Some(identity)
                }
                Phase::Intent | Phase::Released { identity: None, .. }
                    if current.init.is_none() =>
                {
                    None
                }
                _ => return Err(refused(key, LifecycleCause::Uncertain)),
            };
            let native = transaction.attach_managed(identity)?;
            let terminal = native
                .terminal()
                .map_err(|why| refusal("observing cold shared parent scope", &why))?;
            if terminal.is_some_and(|end| end.scope == shards_api::TerminalScope::Released) {
                return Ok(None);
            }
            Err(refused(key, LifecycleCause::Uncertain))
        })
    }

    /// Native VM/helpers counters for the existing host usage sampler. They are
    /// a different measurement plane from each inner OCI workload's counters.
    pub fn backing_usage(&self, key: &BackingKey) -> Result<Usage, Refusal> {
        let key = *key;
        if self.shared_key()? != Some(key) {
            return self.gpu_backing_usage(key);
        }
        let books = self
            .books
            .get()
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        devices::with_pool_transaction(books, &key, &[], |transaction| {
            let record = transaction
                .read_pool(&key)?
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            record.check()?;
            if !matches!(
                record.phase,
                BackingPhase::Granted | BackingPhase::Live | BackingPhase::Draining
            ) {
                return Err(refused(key, LifecycleCause::Pending));
            }
            transaction.managed_usage()
        })
    }

    pub(super) fn add_offers(
        &self,
        capacity: &mut hex_api::Capacity,
        class: Class,
    ) -> Result<(), Refusal> {
        use hex_api::{GpuEligibility, RouteOffer};
        capacity.check()?;
        // Metadata-only reconciliation is outside the family fence/native
        // callbacks. Index still grants and charges a new scope before boot.
        self.prepare_gpu_backing()?;
        let allocation = self.configured_allocation()?;
        if capacity.routes.is_empty() {
            capacity
                .device_routes
                .try_reserve_exact(capacity.devices.len())
                .map_err(|_| Refusal::Exhausted {
                    what: "physical route eligibility",
                    bound: hex_api::MAX_DEVICES,
                })?;
            capacity.device_routes.resize(capacity.devices.len(), 0);
        }
        if class == Class::Oci {
            let key = self.shared_key()?.ok_or_else(|| {
                route::invalid("prepare shared backing before advertising its route")
            })?;
            let record = self.pool_record(key)?;
            if record.execution != Execution::ShardsShared || !record.devices.is_empty() {
                return Err(refused(key, LifecycleCause::Uncertain));
            }
            if !capacity.backings.iter().any(|backing| backing.key == key) {
                capacity
                    .backings
                    .try_reserve_exact(1)
                    .map_err(|_| Refusal::Exhausted {
                        what: "initialized native backings",
                        bound: hex_api::MAX_RUNTIME_ROUTES,
                    })?;
                let mut offer = record.offer()?;
                offer.guest = self.measured_guest(key)?;
                capacity.backings.push(offer);
            }
            push_offer(
                capacity,
                RouteOffer {
                    key: backing_route_keys(Execution::ShardsShared, &[])?.0,
                    cost: None,
                    backing: Some(key),
                },
                &[],
            )?;
        } else if class == Class::Shards {
            push_offer(
                capacity,
                RouteOffer {
                    key: guest_route_key(class, Execution::ShardsDedicated, GpuEligibility::None)?,
                    cost: Some(allocation.cost),
                    backing: None,
                },
                &[],
            )?;
        } else {
            return Err(route::invalid(
                "Shards offers require OCI or explicit Shards class",
            ));
        }
        // Prepared full IDs are provisioning claims only. Bootstrap may reserve
        // them without a running GPU provider; actual DeviceInfo/route bits wait
        // for charged native+guest readiness, never a fabricated cold inventory.
        if self.gpu_backing_definition()?.is_some() {
            let authority = self.authority()?;
            let records = authority.records()?;
            let pools = authority.pool_records()?;
            let current = self.gpu_fresh_candidate(&records, &pools)?;
            for backing in pools
                .iter()
                .filter(|row| row.execution == Execution::ShardsDedicated)
            {
                let definition = self.gpu_definition_for_record(backing)?;
                self.preflight_backing(backing)?;
                if !capacity
                    .backings
                    .iter()
                    .any(|known| known.key == backing.key)
                {
                    capacity
                        .backings
                        .try_reserve_exact(1)
                        .map_err(|_| Refusal::Exhausted {
                            what: "initialized GPU backing",
                            bound: hex_api::MAX_RUNTIME_ROUTES,
                        })?;
                    capacity.backings.push(backing.offer()?);
                }
                let eligible = if current.is_some_and(|row| row.key == backing.key)
                    && backing.phase == BackingPhase::Live
                {
                    match self.checked_physical(&definition.devices, &definition.physical) {
                        Ok(eligible) => eligible,
                        Err(why) => {
                            let unavailable =
                                hex_api::BackendUnavailable::from_refusal(Class::Shards, why)?;
                            if !matches!(&unavailable.cause, Refusal::Backing { key, .. } if *key == backing.key)
                            {
                                return Err(unavailable.cause);
                            }
                            Vec::new()
                        }
                    }
                } else {
                    Vec::new()
                };
                push_offer(
                    capacity,
                    hex_api::RouteOffer {
                        key: guest_route_key(
                            class,
                            Execution::ShardsDedicated,
                            GpuEligibility::Required,
                        )?,
                        cost: Some(allocation.cost),
                        backing: Some(backing.key),
                    },
                    &eligible,
                )?;
            }
        }
        // There is no class-wide VM cost: explicit route+backing describes it.
        capacity.cost = None;
        capacity.check()
    }

    pub(super) fn create_shared(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        let route = spec
            .route
            .ok_or_else(|| route::invalid("shared OCI needs its admitted host route"))?;
        let key = route
            .backing
            .ok_or_else(|| route::invalid("shared OCI needs its admitted backing generation"))?;
        if route.key.execution != Execution::ShardsShared
            || self.shared_key()? != Some(key)
            || !spec.devices.is_empty()
            || spec.allocation.is_some()
        {
            return Err(refused(key, LifecycleCause::Uncertain));
        }
        let books = self
            .books
            .get()
            .ok_or_else(|| refused(key, LifecycleCause::Pending))?;
        devices::with_pool_transaction(books, &key, &[], |transaction| {
            let record = transaction
                .read_pool(&key)?
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            if record.phase != BackingPhase::Live {
                return Err(refused(key, LifecycleCause::Pending));
            }
            let booted = self.shared_running()?;
            self.matches_pool(&booted, &record)?;
            let rewritten = self.rewrite_spec(spec)?;
            self.create_in(&booted, rewritten)
        })
    }

    /// The existing Driver calls this only after observing this exact Released
    /// generation. Cleanup itself returns Released and never clears this binding.
    pub(super) fn acknowledge_shared_release(
        &self,
        key: BackingKey,
    ) -> Result<BackingOffer, Refusal> {
        if self.shared_key()? != Some(key) {
            return Err(refused(key, LifecycleCause::Uncertain));
        }
        let books = self
            .books
            .get()
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        devices::with_pool_transaction(books, &key, &[], |transaction| {
            let record = transaction
                .read_pool(&key)?
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            self.preflight_backing(&record)?;
            if record.execution != Execution::ShardsShared
                || !record.devices.is_empty()
                || record.phase != BackingPhase::Released
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
            let path = self.pool_path()?;
            if let Some(receipt) = pool::read(path)? {
                receipt.matches(&record)?;
                if !matches!(receipt.phase, Phase::Released { .. }) {
                    return Err(refused(key, LifecycleCause::Uncertain));
                }
                transaction.finalize_managed(receipt.released_identity()?)?;
                remove_receipt(path)?;
            }
            let offer = record.offer()?;
            transaction.remove_pool(&key)?;
            self.clear_shared_key(key)?;
            Ok(offer)
        })
    }

    pub(super) fn release_pool(&self, key: BackingKey) -> Result<BackingOffer, Refusal> {
        if self.shared_key()? != Some(key) {
            return Err(refused(key, LifecycleCause::Uncertain));
        }
        let books = self
            .books
            .get()
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        devices::with_pool_transaction(books, &key, &[], |transaction| {
            let mut record = transaction
                .read_pool(&key)?
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            if record.execution != Execution::ShardsShared || !record.devices.is_empty() {
                return Err(refused(key, LifecycleCause::Uncertain));
            }
            if record.phase == BackingPhase::Released {
                return record.offer();
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
            self.stop_and_release(transaction, &mut record, &|record| {
                self.preflight_backing(record)
            })?;
            record.offer()
        })
    }

    /// Stops and releases backing `record`'s scope through its receipt, in `transaction` (its pool turn): a Prepared
    /// one, with no receipt, checked by `prepared` and no native submission proved, is released as it is; any other is
    /// drained, its scope stopped where its receipt names one and not already released, its controller finalized, and
    /// its exit recorded; and its receipt removed.
    fn stop_and_release(
        &self,
        transaction: &dyn Transaction,
        record: &mut PoolRecord,
        prepared: &dyn Fn(&PoolRecord) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        let key = record.key;
        let path = self.pool_path()?;
        if record.phase == BackingPhase::Prepared {
            if pool::read(path)?.is_some() {
                return Err(refused(key, LifecycleCause::Uncertain));
            }
            prepared(record)?;
            transaction.check_unsubmitted_pool()?;
            record.phase = BackingPhase::Released;
            return transaction.put_pool(record);
        }
        let mut receipt = match pool::read(path)? {
            Some(receipt) => receipt,
            None if record.init.is_none() => {
                let mut original = PoolRecord::decode(&record.encode()?)?;
                original.phase = BackingPhase::Granted;
                original.exit = None;
                Receipt::retained(original, Vec::new())?
            }
            None => return Err(refused(key, LifecycleCause::Uncertain)),
        };
        receipt.matches(record)?;
        record.phase = BackingPhase::Draining;
        transaction.put_pool(record)?;
        let identity = match &receipt.phase {
            Phase::Running(identity)
            | Phase::Stopping(identity)
            | Phase::Released {
                identity: Some(identity),
                ..
            } => {
                managed::same_native(NativeRecord::Pool(record), identity)?;
                Some(identity)
            }
            Phase::Intent | Phase::Released { identity: None, .. } if record.init.is_none() => None,
            _ => return Err(refused(key, LifecycleCause::Uncertain)),
        };
        let terminal = match &receipt.phase {
            Phase::Released { status, .. } => *status,
            _ => {
                let identity = identity.map(copy_identity).transpose()?;
                if let Some(identity) = &identity {
                    receipt.phase = Phase::Stopping(copy_identity(identity)?);
                    pool::write(path, &receipt)?;
                }
                let terminal = managed::stop_scope(
                    transaction,
                    identity.as_ref(),
                    self.first_dial_bound()?,
                    refused(key, LifecycleCause::Uncertain),
                )?;
                receipt.phase = Phase::Released {
                    identity,
                    status: terminal.status,
                };
                pool::write(path, &receipt)?;
                terminal.status
            }
        };
        let identity = match &receipt.phase {
            Phase::Released { identity, .. } => identity.as_ref(),
            _ => return Err(refused(key, LifecycleCause::Uncertain)),
        };
        transaction.finalize_managed(identity)?;
        record.exit = Some(exit_of(terminal));
        record.phase = BackingPhase::Released;
        transaction.put_pool(record)?;
        {
            let mut state = self.lock()?;
            if state
                .as_ref()
                .is_some_and(|booted| booted.backing.is_some_and(|backing| backing.key == key))
            {
                *state = None;
            }
        }
        remove_receipt(path)?;
        Ok(())
    }
}

fn remove_receipt(path: &Path) -> Result<(), Refusal> {
    held::remove(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| route::invalid("shared backing receipt has no parent"))?;
    held::sync_folder(parent)
        .map_err(|why| Refusal::os("syncing removed shared guest receipt", why))
}

pub(super) fn copy_identity(
    identity: &shards_api::Identity,
) -> Result<shards_api::Identity, Refusal> {
    Ok(shards_api::Identity {
        pid: identity.pid,
        started: identity.started,
        vsock: super::dedicated::copy_path(&identity.vsock)?,
    })
}

pub(super) fn exit_of(status: shards_api::ManagedStatus) -> hex_api::Exit {
    match status {
        shards_api::ManagedStatus::Code(code) => hex_api::Exit::Code(code),
        shards_api::ManagedStatus::Signal(signal) => hex_api::Exit::Signal(signal),
        shards_api::ManagedStatus::Unknown => hex_api::Exit::Unknown,
    }
}

fn push_offer(
    capacity: &mut hex_api::Capacity,
    offer: hex_api::RouteOffer,
    eligible: &[hex_api::DeviceInfo],
) -> Result<(), Refusal> {
    offer.check()?;
    if capacity
        .routes
        .iter()
        .any(|known| known.key == offer.key && known.backing == offer.backing)
    {
        return Err(route::invalid(
            "an initialized route was advertised more than once",
        ));
    }
    if capacity.routes.len() >= hex_api::MAX_RUNTIME_ROUTES {
        return Err(Refusal::Exhausted {
            what: "initialized runtime routes",
            bound: hex_api::MAX_RUNTIME_ROUTES,
        });
    }
    if capacity.device_routes.len() != capacity.devices.len() {
        return Err(route::invalid(
            "route eligibility is not paired with canonical devices",
        ));
    }
    if capacity.devices.len() > hex_api::MAX_DEVICES || eligible.len() > hex_api::MAX_DEVICES {
        return Err(Refusal::Full(hex_api::MAX_DEVICES));
    }
    let exhausted = || Refusal::Exhausted {
        what: "checked physical capacity slots",
        bound: hex_api::MAX_DEVICES,
    };
    let mut additional = Vec::new();
    if !eligible.is_empty() {
        if eligible
            .windows(2)
            .any(|pair| matches!(pair, [left, right] if left.id >= right.id))
        {
            return Err(route::invalid(
                "checked physical facts are not canonical sorted unique identities",
            ));
        }
        let mut existing = Vec::new();
        existing
            .try_reserve_exact(capacity.devices.len())
            .map_err(|_| exhausted())?;
        for (at, device) in capacity.devices.iter().enumerate() {
            device.check()?;
            existing.push((device.id, at));
        }
        existing.sort_unstable_by_key(|(id, _)| *id);
        if existing
            .windows(2)
            .any(|pair| matches!(pair, [left, right] if left.0 == right.0))
        {
            return Err(route::invalid("capacity repeated a physical identity"));
        }
        let mut absent = 0usize;
        for device in eligible {
            device.check()?;
            if let Ok(at) = existing.binary_search_by_key(&device.id, |(id, _)| *id) {
                let (_, at) = existing
                    .get(at)
                    .ok_or_else(|| route::invalid("physical capacity index changed"))?;
                if capacity.devices.get(*at) != Some(device) {
                    return Err(route::invalid(
                        "checked physical metadata or holder conflicts with capacity",
                    ));
                }
            } else {
                absent = absent.checked_add(1).ok_or_else(exhausted)?;
            }
        }
        if capacity
            .devices
            .len()
            .checked_add(absent)
            .is_none_or(|count| count > hex_api::MAX_DEVICES)
        {
            return Err(Refusal::Full(hex_api::MAX_DEVICES));
        }
        additional
            .try_reserve_exact(absent)
            .map_err(|_| exhausted())?;
        for device in eligible {
            if existing
                .binary_search_by_key(&device.id, |(id, _)| *id)
                .is_err()
            {
                additional.push(device.try_clone()?);
            }
        }
    }
    let bit = u32::try_from(capacity.routes.len())
        .ok()
        .and_then(|position| 1u64.checked_shl(position))
        .ok_or_else(|| route::invalid("route eligibility word overflows"))?;
    capacity
        .routes
        .try_reserve_exact(1)
        .map_err(|_| Refusal::Exhausted {
            what: "initialized runtime routes",
            bound: hex_api::MAX_RUNTIME_ROUTES,
        })?;
    // Complete every fallible clone/reservation before publishing any slot,
    // route or mask. Existing native order, metadata and holders stay exact.
    capacity
        .devices
        .try_reserve_exact(additional.len())
        .map_err(|_| exhausted())?;
    capacity
        .device_routes
        .try_reserve_exact(additional.len())
        .map_err(|_| exhausted())?;
    capacity.routes.push(offer);
    for device in additional {
        capacity.devices.push(device);
        capacity.device_routes.push(0);
    }
    for (device, mask) in capacity.devices.iter().zip(&mut capacity.device_routes) {
        if eligible
            .binary_search_by_key(&device.id, |candidate| candidate.id)
            .is_ok()
        {
            *mask |= bit;
        }
    }
    Ok(())
}

/// A shared backing's guest's own overhead after one capacity answer (joint plan step 14, C7; ROOT, 2026-11-13): what
/// its agent says it uses (its total memory less its free) past what its boxes hold, `boxes_memory`, the most seen,
/// only rising from `kept`. Its CPU and disk its agent's answer doesn't read, so they stay as kept: none.
fn guest_seen(
    kept: hex_api::LearnedOverhead,
    guest: &hex_api::Capacity,
    boxes_memory: u64,
) -> hex_api::LearnedOverhead {
    let own = guest
        .total
        .mem_bytes
        .saturating_sub(guest.free.mem_bytes)
        .saturating_sub(boxes_memory);
    hex_api::LearnedOverhead {
        mem_bytes: Some(kept.mem_bytes.map_or(own, |seen| seen.max(own))),
        ..kept
    }
}

#[cfg(test)]
#[path = "shared_tests.rs"]
mod tests;
