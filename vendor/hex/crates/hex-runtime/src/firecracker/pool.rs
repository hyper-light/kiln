//! A charged cold observer is a canonical native PoolRecord, never a workload or inventory cache.

use super::*;
use hex_api::{
    BackingGrant, BackingKey, BackingOffer, BackingPhase, CapacityResources, Execution,
    PhysicalDevice, RuntimeCost,
};
use hex_core::record::{NativeRecord, PoolRecord};
use sha2::{Digest, Sha256};

fn refused(key: BackingKey, cause: LifecycleCause) -> Refusal {
    Refusal::Backing { key, cause }
}

fn observer_cost(config: &Config) -> Result<RuntimeCost, Refusal> {
    let cost = RuntimeCost {
        ceiling: config.cost.floor,
        ..config.cost
    };
    cost.check()?;
    let allocation = cost.allocate(CapacityResources::default())?;
    allocation.guest()?;
    Ok(cost)
}

pub(super) fn observer_configuration(
    config: &Config,
    claims: &[DeviceId],
) -> Result<[u8; 32], Refusal> {
    config.check()?;
    if claims.len() > hex_api::MAX_DEVICES
        || claims
            .windows(2)
            .any(|pair| matches!(pair, [a,b] if a >= b))
    {
        return Err(invalid(
            "observer claims must be bounded canonical unique GPU identities",
        ));
    }
    let cost = observer_cost(config)?;
    let mut hash = Sha256::new();
    let mut part = |bytes: &[u8]| -> Result<(), Refusal> {
        hash.update(
            u64::try_from(bytes.len())
                .map_err(|_| invalid("observer configuration length overflows"))?
                .to_be_bytes(),
        );
        hash.update(bytes);
        Ok(())
    };
    part(b"hex-firecracker-observer/v1")?;
    part(config.pool_id.as_bytes())?;
    part(&config.binary_sha256)?;
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
        &config.binary,
        &config.kernel,
        &config.initrd,
        &config.state_dir,
    ] {
        part(path.as_os_str().as_encoded_bytes())?;
    }
    for id in claims {
        if id.kind != DeviceKind::Gpu {
            return Err(invalid("observer provisioning claim is not a GPU"));
        }
        part(&id.key.to_be_bytes())?;
    }
    // A video BIOS image is part of what the observer VM boots with; none adds nothing, so a
    // configuration without one keeps its identity.
    for image in &config.vbios {
        part(image.sbdf.as_bytes())?;
        part(&image.sha256)?;
    }
    Ok(hash.finalize().into())
}

/// Pure temporary proposal; only existing Books/Store may publish its selected canonical row.
enum ObserverProposal<'a> {
    Empty,
    AwaitAck(&'a PoolRecord),
    Prepare {
        devices: Vec<DeviceId>,
        current: Option<&'a PoolRecord>,
    },
}

impl FirecrackerRuntime {
    pub(super) fn observer_claims(&self) -> Result<Vec<DeviceId>, Refusal> {
        let mut claims = Vec::new();
        let count = self
            .catalog
            .entries_for(Route::Vfio)
            .filter(|entry| entry.info.id.kind == DeviceKind::Gpu)
            .count();
        if count > hex_api::MAX_DEVICES {
            return Err(exhausted("observer physical claims", hex_api::MAX_DEVICES));
        }
        claims
            .try_reserve_exact(count)
            .map_err(|_| exhausted("observer physical claims", count))?;
        claims.extend(
            self.catalog
                .entries_for(Route::Vfio)
                .filter(|entry| entry.info.id.kind == DeviceKind::Gpu)
                .map(|entry| entry.info.id),
        );
        claims.sort_unstable();
        Ok(claims)
    }

    /// Metadata-only proposal on the SAME initialized owner. The guarded callback recomputes
    /// the one identity complement; no readings, grants or native effect are inferred here.
    pub fn prepare_backing(&self) -> Result<Option<BackingOffer>, Refusal> {
        self.require_initialized()?;
        let configured = self.observer_claims()?;
        if configured.is_empty() {
            return Ok(None);
        }
        let authority = self.authority()?;
        let records = authority.records()?;
        let pools = authority.pool_records()?;
        let (devices, current) = match self.observer_proposal(&configured, &records, &pools)? {
            ObserverProposal::Empty => return Ok(None),
            ObserverProposal::AwaitAck(record) => return record.offer().map(Some),
            ObserverProposal::Prepare { devices, current } => (devices, current),
        };
        let configuration = observer_configuration(&self.config, &configured)?;
        // An empty desired set is only a changed-current drain/cancel proposal. With no current
        // row it returned Empty above; it never becomes an empty GPU PoolRecord/route/offer.
        let pool = observer_configuration(&self.config, &devices)?;
        let cost = observer_cost(&self.config)?;
        let mut validate = |records: &crate::devices::Records, pools: &[PoolRecord]| match self
            .observer_proposal(&configured, records, pools)?
        {
            ObserverProposal::Prepare {
                devices: now,
                current: retained,
            } if now == devices && retained == current => Ok(()),
            _ => Err(match current {
                Some(record) => refused(record.key, LifecycleCause::Busy),
                None => invalid("canonical observer complement changed before publication"),
            }),
        };
        let canonical = authority.prepare_pool(
            pool,
            Execution::Firecracker,
            cost,
            &devices,
            (&configuration, &configured, current),
            &mut validate,
        )?;
        self.preflight_backing(&canonical)?;
        if canonical.cost != cost
            || match current {
                Some(before) => {
                    canonical.key != before.key
                        || canonical.devices != before.devices
                        || canonical.allocation != before.allocation
                        || canonical.init != before.init
                }
                None => {
                    canonical.key.pool != pool
                        || canonical.devices != devices
                        || canonical.phase != BackingPhase::Prepared
                }
            }
        {
            return Err(refused(canonical.key, LifecycleCause::Uncertain));
        }
        if canonical.phase == BackingPhase::Prepared && canonical.devices != devices {
            // No native I/O runs while the family metadata fence is held. Cancellation owes
            // local no-Intent AND the actual same-native-owner no-submission observation.
            return self.release_observer(&canonical.key).map(Some);
        }
        canonical.offer().map(Some)
    }

    fn observer_proposal<'a>(
        &self,
        configured: &[DeviceId],
        records: &crate::devices::Records,
        pools: &'a [PoolRecord],
    ) -> Result<ObserverProposal<'a>, Refusal> {
        if records.len() > crate::MAX_BOXES || pools.len() > crate::MAX_BOXES {
            return Err(Refusal::Full(crate::MAX_BOXES));
        }
        let mut family = Vec::new();
        family
            .try_reserve_exact(pools.len())
            .map_err(|_| exhausted("observer family snapshot", pools.len()))?;
        for pool in pools
            .iter()
            .filter(|pool| pool.execution == Execution::Firecracker)
        {
            self.preflight_backing(pool)?;
            family.push(pool);
        }
        family.sort_unstable_by_key(|pool| pool.key);
        let mut references = Vec::new();
        references
            .try_reserve_exact(records.len())
            .map_err(|_| exhausted("observer consuming references", records.len()))?;
        for (id, boxed) in records {
            let boxed = boxed.as_ref().map_err(|_| uncertain(id))?;
            boxed.spec.check()?;
            let Some(route) = boxed.spec.route.filter(|route| {
                route.key.execution == Execution::Firecracker && route.backing.is_some()
            }) else {
                continue;
            };
            let key = route.backing.ok_or_else(|| uncertain(id))?;
            let source = family
                .binary_search_by_key(&key, |pool| pool.key)
                .ok()
                .and_then(|at| family.get(at))
                .copied()
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            route.check_backing_parts(
                boxed.spec.allocation.map(|allocation| allocation.cost),
                key,
                source.cost,
                &source.devices,
                &boxed.spec.devices,
            )?;
            references.push(key);
        }
        references.sort_unstable();
        if let Some(pair) = references
            .windows(2)
            .find(|pair| matches!(pair, [a,b] if a == b))
        {
            let key = pair
                .first()
                .copied()
                .ok_or_else(|| invalid("missing consuming reference"))?;
            return Err(refused(key, LifecycleCause::Uncertain));
        }
        let mut current: Option<&PoolRecord> = None;
        let mut ack = None;
        for pool in family {
            if references.binary_search(&pool.key).is_ok() {
                continue;
            }
            if pool.phase == BackingPhase::Released {
                if ack.is_none() {
                    ack = Some(pool);
                }
            } else if current.replace(pool).is_some() {
                return Err(refused(pool.key, LifecycleCause::Uncertain));
            }
        }
        let devices =
            self.catalog
                .unheld_subset(configured, current.map(|pool| pool.key), records, pools)?;
        // A historical configuration with no references still needs Index ACK/removal. Let its
        // ordinary Released queue run before replacing a changed current configuration; otherwise
        // a full-P parent can collide with the same hash of the proposed union.
        if current.is_none_or(|pool| pool.devices != devices)
            && let Some(ack) = ack
        {
            return Ok(ObserverProposal::AwaitAck(ack));
        }
        if current.is_none() && devices.is_empty() {
            return Ok(ObserverProposal::Empty);
        }
        Ok(ObserverProposal::Prepare { devices, current })
    }

    pub(super) fn fresh_observer(
        &self,
        records: &crate::devices::Records,
        pools: &[PoolRecord],
    ) -> Result<Option<BackingKey>, Refusal> {
        Ok(
            match self.observer_proposal(&self.observer_claims()?, records, pools)? {
                ObserverProposal::Prepare {
                    devices,
                    current: Some(record),
                } if record.phase == BackingPhase::Live && record.devices == devices => {
                    Some(record.key)
                }
                _ => None,
            },
        )
    }

    /// Whether `record` is this configuration's observer as its node recorded it under its profile's census cost
    /// (joint plan step 14): its cost the profile's but for its census numbers, and its identity this configuration's
    /// recomputed under that cost. Such an observer is retired ([`Self::retire_census_observers`]) before any exact
    /// check sees it.
    fn census_observer(&self, record: &PoolRecord) -> Result<bool, Refusal> {
        let current = observer_cost(&self.config)?;
        if record.execution != Execution::Firecracker
            || record.cost == current
            || record.cost.census_free() != current
        {
            return Ok(false);
        }
        let recorded = Config {
            cost: RuntimeCost {
                guest_overhead: record.cost.guest_overhead,
                overhead: record.cost.overhead,
                ..self.config.cost
            },
            ..self.config.clone()
        };
        Ok(record.key.pool == observer_configuration(&recorded, &record.devices)?)
    }

    /// Pure second pass for the same owner's all-record preflight, before any native callback. An observer its node
    /// recorded under the profile's census cost passes, retired at the node's start ([`Self::census_observer`]).
    pub fn preflight_backing(&self, record: &PoolRecord) -> Result<(), Refusal> {
        record.check()?;
        if self.census_observer(record)? {
            return Ok(());
        }
        let bad = || refused(record.key, LifecycleCause::Uncertain);
        if record.execution != Execution::Firecracker
            || record.cost != observer_cost(&self.config)?
            || record.key.pool != observer_configuration(&self.config, &record.devices)?
        {
            return Err(bad());
        }
        hex_api::BackingOffer::check_selection_parts(&self.observer_claims()?, &record.devices)?;
        if let Some(allocation) = record.allocation
            && allocation != record.cost.allocate(CapacityResources::default())?
        {
            return Err(bad());
        }
        Ok(())
    }

    pub(super) fn observer_records(&self) -> Result<Vec<PoolRecord>, Refusal> {
        let mut records = self.authority()?.pool_records()?;
        if records.len() > crate::MAX_BOXES {
            return Err(Refusal::Full(crate::MAX_BOXES));
        }
        // Validate ALL configured family rows before any caller can make a native callback.
        for record in records
            .iter()
            .filter(|record| record.execution == Execution::Firecracker)
        {
            self.preflight_backing(record)?;
        }
        records.retain(|record| record.execution == Execution::Firecracker);
        records.sort_unstable_by_key(|record| record.key);
        if records
            .windows(2)
            .any(|pair| matches!(pair,[a,b] if a.key.pool == b.key.pool))
        {
            return Err(invalid("observer family repeats a canonical configuration"));
        }
        Ok(records)
    }

    fn observer_record(&self, key: &BackingKey) -> Result<PoolRecord, Refusal> {
        self.observer_records()?
            .into_iter()
            .find(|record| record.key == *key)
            .ok_or_else(|| refused(*key, LifecycleCause::Uncertain))
    }

    fn pool_paths(&self, record: &PoolRecord) -> Result<Paths, Refusal> {
        let scope = crate::devices::managed_native_scope_id(NativeRecord::Pool(record))?;
        self.scope_paths(&scope)
    }

    fn pool_identity(&self, record: &PoolRecord) -> Result<Option<shards_api::Identity>, Refusal> {
        record
            .init
            .map(|init| {
                Ok(shards_api::Identity {
                    pid: init.pid,
                    started: init.start_ticks,
                    vsock: self.pool_paths(record)?.vsock,
                })
            })
            .transpose()
    }

    fn pool_client(
        &self,
        record: &PoolRecord,
        transaction: &dyn Transaction,
        until: Option<Instant>,
    ) -> Result<Client, Refusal> {
        self.preflight_backing(record)?;
        if !matches!(
            record.phase,
            BackingPhase::Granted | BackingPhase::Live | BackingPhase::Draining
        ) || record.init.is_none()
        {
            return Err(refused(record.key, LifecycleCause::Pending));
        }
        let identity = self.pool_identity(record)?;
        let native = transaction.attach_managed(identity.as_ref())?;
        if native
            .terminal()
            .map_err(|_| refused(record.key, LifecycleCause::Uncertain))?
            .is_some()
        {
            return Err(refused(record.key, LifecycleCause::Uncertain));
        }
        let link = own_link(transport::Endpoint {
            path: self.pool_paths(record)?.vsock,
            timeout: self.config.operation_timeout,
            create_deadline: until,
        })?;
        match until {
            Some(until) => Client::connect_before(link, until),
            None => Client::connect(link),
        }
    }

    fn pool_observations(
        &self,
        record: &PoolRecord,
        client: &Client,
        until: Option<Instant>,
    ) -> Result<Vec<PhysicalDevice>, Refusal> {
        let paths = self.pool_paths(record)?;
        self.native_observations(&record.devices, &paths, client, None, until)
    }

    fn native_observations(
        &self,
        selected: &[DeviceId],
        paths: &Paths,
        client: &Client,
        owner: Option<&BoxId>,
        until: Option<Instant>,
    ) -> Result<Vec<PhysicalDevice>, Refusal> {
        let mut indexed = Vec::new();
        indexed
            .try_reserve_exact(self.catalog.entries().len())
            .map_err(|_| exhausted("observer catalog index", self.catalog.entries().len()))?;
        indexed.extend(self.catalog.entries());
        indexed.sort_unstable_by_key(|entry| entry.info.id);
        let mut assignments = Vec::new();
        assignments
            .try_reserve_exact(selected.len())
            .map_err(|_| exhausted("observer PCI witnesses", selected.len()))?;
        for selected in selected.iter().filter(|id| id.kind == DeviceKind::Gpu) {
            let entry = indexed
                .binary_search_by_key(selected, |entry| entry.info.id)
                .ok()
                .and_then(|at| indexed.get(at))
                .ok_or_else(|| {
                    invalid("native observer selection has no exact catalog identity")
                })?;
            let Some(Binding::Vfio { sbdf, .. }) = entry.binding(Route::Vfio) else {
                return Err(invalid("native observer identity has no VFIO route"));
            };
            let id = vfio::function_id(sbdf)?;
            let witness =
                transport::attachment(&paths.api, &id, self.config.operation_timeout, until)?;
            if witness.id != id || witness.sbdf != *sbdf {
                return Err(invalid(
                    "native observer VMM witness changes physical identity",
                ));
            }
            let guest = hex_gpu::PciAddress::parse(&witness.guest_sbdf)
                .ok_or_else(|| invalid("native observer VMM guest PCI identity is malformed"))?;
            assignments.push(observations::Assignment {
                host: *selected,
                guest: DeviceId {
                    kind: DeviceKind::Gpu,
                    key: hex_gpu::Identity::Pci(guest).key(),
                },
            });
        }
        let report = match until {
            Some(until) => client.physical_report_before(until),
            None => client.physical_report(),
        }?;
        let joined = observations::join(&assignments, &report)?;
        let capacity = match until {
            Some(until) => client.capacity_before(Class::Oci, until),
            None => client.capacity(Class::Oci),
        }?
        .ok_or_else(|| invalid("native observer guest has no OCI attachment capacity"))?;
        observations::check_bindings(&assignments, &report, &capacity, owner)?;
        if let Some(until) = until {
            remaining(until)?;
        }
        Ok(joined)
    }

    /// Transient factual native observation, with no capacity/report recursion or cached inventory.
    /// The outer Node supplies its existing authority; this adapter does not grant any holder here.
    pub fn physical_observations(&self) -> Result<Vec<PhysicalDevice>, Refusal> {
        self.physical_observations_before(None)
    }

    pub(super) fn physical_observations_before(
        &self,
        until: Option<Instant>,
    ) -> Result<Vec<PhysicalDevice>, Refusal> {
        let mut observed = Vec::new();
        for before in self
            .observer_records()?
            .into_iter()
            .filter(|record| record.phase == BackingPhase::Live)
        {
            let keys = self.catalog.conflict_ids(&before.devices)?;
            let next = crate::devices::with_pool_transaction(
                &self.books,
                &before.key,
                &keys,
                |transaction| {
                    let record = transaction
                        .read_pool(&before.key)?
                        .ok_or_else(|| refused(before.key, LifecycleCause::Uncertain))?;
                    if record != before {
                        return Err(refused(before.key, LifecycleCause::Uncertain));
                    }
                    let client = self.pool_client(&record, transaction, until)?;
                    self.pool_observations(&record, &client, until)
                },
            )?;
            append_observations(&mut observed, next)?;
        }
        // After observer handoff a retained GPU Box is the actual current driver provider. Read
        // each through its same method/device turns and managed receipt; never repick or cache.
        for (id, record) in self.authority()?.records()? {
            let record = record.map_err(|_| uncertain(&id))?;
            if record.spec.backend_class()? != Class::Firecracker
                || record.phase != Phase::Ready
                || !record
                    .spec
                    .devices
                    .iter()
                    .any(|id| id.kind == DeviceKind::Gpu)
            {
                continue;
            }
            let next = self.with_record(&id, |record, transaction| {
                let client = self.client_before(record, transaction, until)?;
                self.native_observations(
                    &record.spec.devices,
                    &self.paths(record)?,
                    &client,
                    Some(&id),
                    until,
                )
            })?;
            append_observations(&mut observed, next)?;
        }
        observed.sort_unstable_by_key(|device| device.info.id);
        if observed
            .windows(2)
            .any(|pair| matches!(pair, [a,b] if a.info.id == b.info.id))
        {
            return Err(invalid("two native scopes report the same physical GPU"));
        }
        Ok(observed)
    }

    pub(super) fn initialize_observer(&self, grant: BackingGrant) -> Result<BackingOffer, Refusal> {
        grant.check()?;
        let keys = self.catalog.conflict_ids(&grant.devices)?;
        let until = Instant::now()
            .checked_add(self.config.boot_timeout)
            .ok_or_else(|| invalid("observer boot deadline overflows"))?;
        crate::devices::with_pool_transaction(&self.books, &grant.key, &keys, |transaction| {
            let before = transaction
                .read_pool(&grant.key)?
                .ok_or_else(|| refused(grant.key, LifecycleCause::Pending))?;
            self.preflight_backing(&before)?;
            if before.phase != BackingPhase::Prepared {
                // A repeated/interrupted Intent never starts another incarnation. Only a positively
                // observed retained Live scope may answer idempotently.
                if before.phase != BackingPhase::Live {
                    return Err(refused(grant.key, LifecycleCause::Uncertain));
                }
                let mut accepted = before.offer()?;
                let retained = before
                    .allocation
                    .ok_or_else(|| refused(grant.key, LifecycleCause::Uncertain))?;
                if retained != grant.allocation || before.devices != grant.devices {
                    return Err(refused(grant.key, LifecycleCause::Uncertain));
                }
                let client = self.pool_client(&before, transaction, Some(until))?;
                self.pool_observations(&before, &client, Some(until))?;
                accepted.phase = BackingPhase::Live;
                return Ok(accepted);
            }
            // Its VMM pins all its RAM for DMA, the observer VM given its GPUs whole: refused before anything is
            // granted or launched where its memlock can't hold that (vfio::Host::dma_memlock).
            let memlock = if grant.devices.is_empty() {
                None
            } else {
                self.host.dma_memlock(grant.allocation.guest()?.mem_bytes)?
            };
            let record = transaction.grant_pool(&grant)?;
            self.preflight_backing(&record)?;
            self.config.verify_inputs()?;
            let plan = vfio::plan(&self.host, &self.catalog, &grant.devices, &[])?;
            // A function no reset clears goes to one VM per host boot, an observer as much as a box: the pool says so.
            let functions = vfio::functions(&plan)?;
            let once = vfio::once_per_boot(&self.host, &functions)?;
            if let Some((function, spent)) = self.unavailable_once(&once)? {
                return Err(single_use::pool_refused(
                    "a GPU pool's observer (VFIO)",
                    &grant.key,
                    function,
                    &spent,
                    self.boot.as_ref(),
                ));
            }
            let marks = single_use::Marks::of(&self.marks.dir, self.boot.as_ref())?;
            let paths = self.pool_paths(&record)?;
            match fs::symlink_metadata(&paths.dir) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Refusal::os("checking observer incarnation path", error)),
                Ok(_) => return Err(refused(grant.key, LifecycleCause::Uncertain)),
            }
            // The observer VM owns its own attached GPUs; no Box runs in it.
            let owner = crate::devices::managed_native_scope_id(NativeRecord::Pool(&record))?;
            let vm = self.config.machine_guest(
                grant.allocation.guest()?,
                &paths.vsock,
                &plan,
                &owner,
            )?;
            let program = self.vm_program(&paths)?;
            let (io, mut output) = Self::vm_io()?;
            fs::create_dir(&paths.dir)
                .and_then(|()| fs::set_permissions(&paths.dir, fs::Permissions::from_mode(0o700)))
                .map_err(|error| Refusal::os("creating observer incarnation directory", error))?;
            write_config(&paths.config, &vm)?;
            let ready = UnixListener::bind(&paths.ready)
                .map_err(|error| Refusal::os("binding observer readiness", error))?;
            // Given on this boot before the VMM that opens them starts, so no crash past here forgets it.
            marks.spend(&once)?;
            let native = transaction.launch_managed(
                &ManagedLaunch::Firecracker {
                    program: &program,
                    vsock: &paths.vsock,
                    memlock,
                },
                io,
                remaining(until)?,
            )?;
            let mut current = transaction
                .read_pool(&grant.key)?
                .ok_or_else(|| refused(grant.key, LifecycleCause::Uncertain))?;
            if current.key != record.key
                || current.cost != record.cost
                || current.devices != record.devices
                || current.allocation != record.allocation
                || current.phase != BackingPhase::Granted
                || current.init.is_none()
            {
                return Err(refused(grant.key, LifecycleCause::Uncertain));
            }
            transport::ready(&ready, native.as_ref(), &mut output, until)?;
            drop(output);
            let client = self.pool_client(&current, transaction, Some(until))?;
            self.pool_observations(&current, &client, Some(until))?;
            current.phase = BackingPhase::Live;
            transaction.put_pool(&current)?;
            current.offer()
        })
    }

    pub(super) fn release_observer(&self, key: &BackingKey) -> Result<BackingOffer, Refusal> {
        self.release_observer_before(key, None)
    }

    pub(super) fn release_observer_before(
        &self,
        key: &BackingKey,
        until: Option<Instant>,
    ) -> Result<BackingOffer, Refusal> {
        let before = self.observer_record(key)?;
        let keys = self.catalog.conflict_ids(&before.devices)?;
        crate::devices::with_pool_transaction(&self.books, key, &keys, |transaction| {
            let mut record = transaction
                .read_pool(key)?
                .ok_or_else(|| refused(*key, LifecycleCause::Uncertain))?;
            self.preflight_backing(&record)?;
            if record.phase == BackingPhase::Released {
                return record.offer();
            }
            self.stop_observer(transaction, &mut record, until)?;
            record.offer()
        })
    }

    /// Stops and releases observer `record`'s scope, in `transaction` (its pool turn), by `until` where given: a
    /// Prepared one, its Intent never submitted, is released as it is; any other is drained, its VMM stopped and its
    /// whole scope seen released, its exit recorded, its controller finalized and its folder removed.
    fn stop_observer(
        &self,
        transaction: &dyn Transaction,
        record: &mut PoolRecord,
        until: Option<Instant>,
    ) -> Result<(), Refusal> {
        let key = record.key;
        if record.phase == BackingPhase::Prepared {
            match fs::symlink_metadata(self.pool_paths(record)?.dir) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(Refusal::os("checking unsubmitted observer Intent", error));
                }
                Ok(_) => return Err(refused(key, LifecycleCause::Uncertain)),
            }
            transaction.check_unsubmitted_pool()?;
            record.phase = BackingPhase::Released;
            return transaction.put_pool(record);
        }
        record.phase = BackingPhase::Draining;
        transaction.put_pool(record)?;
        let identity = self.pool_identity(record)?;
        let native = transaction.attach_managed(identity.as_ref())?;
        native
            .stop()
            .map_err(|_| refused(key, LifecycleCause::Uncertain))?;
        let patience = until
            .map(remaining)
            .transpose()?
            .map_or(self.config.stop_timeout, |left| {
                left.min(self.config.stop_timeout)
            });
        let terminal = native
            .wait_for(patience)
            .map_err(|_| refused(key, LifecycleCause::Uncertain))?
            .filter(|end| end.scope == shards_api::TerminalScope::Released)
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        record.exit = Some(exit(terminal.status));
        transaction.put_pool(record)?;
        transaction.finalize_managed(identity.as_ref())?;
        let paths = self.pool_paths(record)?;
        match fs::remove_dir_all(&paths.dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Refusal::os("removing released observer directory", error));
            }
        }
        File::open(&self.config.state_dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| Refusal::os("syncing released observer directory", error))?;
        record.phase = BackingPhase::Released;
        transaction.put_pool(record)
    }

    /// Retires each observer its node recorded under its profile's census cost (joint plan step 14; ROOT, 2026-11-13,
    /// by decision 9's rule: the profile's identity changed with its census-free cost, `RuntimeCost::census_free`),
    /// never leaking it: in its pool turn, each box consuming it is kept retired (`BoxRecord::retire`), every call but
    /// its inspect and its owner's delete refused `Retired`; its VMM is stopped and its scope released as a release's
    /// is; and its record is removed. An observer of another profile is none of this one's, left to the checks that
    /// refuse it.
    /// Retires what this backend's node recorded under its profile's census cost (joint plan step 14; ROOT,
    /// 2026-11-13: no legacy receipt runs on with a census number as its bound), run by its node's preflight before
    /// any exact cost check: each observer and its consumers ([`Self::retire_census_observers`]), then each VM box whose
    /// receipt's cost is the profile's but for its census numbers, its VM stopped and its scope released and the box
    /// kept retired (`BoxRecord::retire`).
    pub fn retire_census(&self) -> Result<(), Refusal> {
        self.retire_census_observers()?;
        for (id, record) in self.authority()?.records()? {
            let record = record.map_err(|_| uncertain(&id))?;
            if record.retired || !matches!(record.spec.class, ClassSpec::Firecracker(_)) {
                continue;
            }
            if !self.census_vm(&record) {
                continue;
            }
            let keys = self.catalog.conflict_ids(&record.spec.devices)?;
            crate::devices::with_transaction(&self.books, &id, &keys, |transaction| {
                let mut current = transaction.read(&id)?.ok_or_else(|| missing(&id))?;
                if current.init.is_some() {
                    self.stop_vm(&mut current, transaction)?;
                }
                current.retire();
                transaction.put(&current)
            })?;
        }
        Ok(())
    }

    /// Whether VM box `record`'s receipt is this configuration's profile under its census numbers (joint plan step 14):
    /// retired at its node's start ([`Self::retire_census`]), so the preflight passes it, and no exact check sees it.
    pub(super) fn census_vm(&self, record: &BoxRecord) -> bool {
        matches!(record.spec.class, ClassSpec::Firecracker(_))
            && record.spec.allocation.is_some_and(|allocation| {
                allocation.cost != self.config.cost
                    && allocation.cost.census_free() == self.config.cost
            })
    }

    fn retire_census_observers(&self) -> Result<(), Refusal> {
        for record in self.authority()?.pool_records()? {
            if !self.census_observer(&record)? {
                continue;
            }
            let key = record.key;
            let keys = self.catalog.conflict_ids(&record.devices)?;
            crate::devices::with_pool_transaction(&self.books, &key, &keys, |transaction| {
                let mut record = transaction
                    .read_pool(&key)?
                    .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
                for (id, boxed) in transaction.records()? {
                    let mut boxed = boxed.map_err(|_| uncertain(&id))?;
                    if boxed.retired
                        || boxed.spec.route.and_then(|route| route.backing) != Some(key)
                    {
                        continue;
                    }
                    // A consumer that launched a VM of its own has it stopped and its scope released first.
                    if boxed.init.is_some() {
                        self.stop_vm(&mut boxed, transaction)?;
                    }
                    boxed.retire();
                    transaction.put(&boxed)?;
                }
                if record.phase != BackingPhase::Released {
                    self.stop_observer(transaction, &mut record, None)?;
                } else if record.allocation.is_some() {
                    transaction.finalize_managed(self.pool_identity(&record)?.as_ref())?;
                }
                transaction.remove_pool(&key)
            })?;
        }
        Ok(())
    }

    /// This second protocol call is the caller's explicit Index ACK, not an inference from
    /// observing Released in capacity. Runtime entry decides that it entered the ACK phase.
    pub(super) fn acknowledge_observer(&self, key: &BackingKey) -> Result<BackingOffer, Refusal> {
        let before = self.observer_record(key)?;
        let keys = self.catalog.conflict_ids(&before.devices)?;
        crate::devices::with_pool_transaction(&self.books, key, &keys, |transaction| {
            let record = transaction
                .read_pool(key)?
                .ok_or_else(|| refused(*key, LifecycleCause::Uncertain))?;
            self.preflight_backing(&record)?;
            if record != before || record.phase != BackingPhase::Released {
                return Err(refused(*key, LifecycleCause::Uncertain));
            }
            let released = record.offer()?;
            for (id, boxed) in transaction.records()? {
                let boxed = boxed.map_err(|_| uncertain(&id))?;
                boxed.spec.check()?;
                if boxed.spec.route.and_then(|route| route.backing) == Some(*key) {
                    // Parent metadata remains the immutable source for its sole Box S lifetime.
                    return Ok(released);
                }
            }
            if record.allocation.is_some() {
                transaction.finalize_managed(self.pool_identity(&record)?.as_ref())?;
            }
            match fs::symlink_metadata(self.pool_paths(&record)?.dir) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(Refusal::os("checking acknowledged observer state", error));
                }
                Ok(_) => return Err(refused(*key, LifecycleCause::Uncertain)),
            }
            // SAME Tx removal revalidates native no-submission for an unallocated canceled
            // lifetime; allocated Released already owes positive full-scope finalization above.
            transaction.remove_pool(key)?;
            Ok(released)
        })
    }

    pub(super) fn observer_usage(&self, key: &BackingKey) -> Result<Usage, Refusal> {
        let before = self.observer_record(key)?;
        let keys = self.catalog.conflict_ids(&before.devices)?;
        crate::devices::with_pool_transaction(&self.books, key, &keys, |transaction| {
            let record = transaction
                .read_pool(key)?
                .ok_or_else(|| refused(*key, LifecycleCause::Uncertain))?;
            self.preflight_backing(&record)?;
            let identity = self.pool_identity(&record)?;
            let _receipt = transaction.attach_managed(identity.as_ref())?;
            transaction.managed_usage()
        })
    }

    /// Recovery never reboots an interrupted Granted intent. A retained Live process is joined;
    /// cleanup releases claims only through the same positive whole-scope terminal witness.
    pub fn recover_backing(&self) -> Result<(), Refusal> {
        self.require_initialized()?;
        for record in self.observer_records()? {
            match record.phase {
                BackingPhase::Prepared | BackingPhase::Released => Ok(()),
                BackingPhase::Granted | BackingPhase::Draining => {
                    self.release_observer(&record.key).map(|_| ())
                }
                BackingPhase::Live => {
                    let keys = self.catalog.conflict_ids(&record.devices)?;
                    let ended = crate::devices::with_pool_transaction(
                        &self.books,
                        &record.key,
                        &keys,
                        |transaction| {
                            let current = transaction
                                .read_pool(&record.key)?
                                .ok_or_else(|| refused(record.key, LifecycleCause::Uncertain))?;
                            if current != record {
                                return Err(refused(record.key, LifecycleCause::Uncertain));
                            }
                            let identity = self.pool_identity(&current)?;
                            let native = transaction.attach_managed(identity.as_ref())?;
                            Ok(native
                                .terminal()
                                .map_err(|_| refused(record.key, LifecycleCause::Uncertain))?
                                .is_some_and(|terminal| {
                                    terminal.scope == shards_api::TerminalScope::Released
                                }))
                        },
                    )?;
                    if ended {
                        self.release_observer(&record.key)?;
                    } else {
                        self.physical_observations()?;
                    }
                    Ok(())
                }
            }?;
        }
        Ok(())
    }
}

fn append_observations(
    observed: &mut Vec<PhysicalDevice>,
    next: Vec<PhysicalDevice>,
) -> Result<(), Refusal> {
    let total = observed
        .len()
        .checked_add(next.len())
        .filter(|count| *count <= hex_api::MAX_DEVICES)
        .ok_or_else(|| exhausted("current native observed GPUs", hex_api::MAX_DEVICES))?;
    observed
        .try_reserve_exact(next.len())
        .map_err(|_| exhausted("current native observed GPUs", total))?;
    observed.extend(next);
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::time::Duration;

    pub(in super::super) fn config() -> Config {
        let guest = CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: 64 << 20,
            disk_bytes: 0,
        };
        Config {
            binary: "/fixture/firecracker".into(),
            binary_sha256: [1; 32],
            kernel: "/fixture/kernel".into(),
            initrd: "/fixture/initrd".into(),
            state_dir: "/fixture/state".into(),
            pool_id: "configured fixture pool".into(),
            boot_timeout: Duration::from_secs(3),
            stop_timeout: Duration::from_secs(1),
            operation_timeout: Duration::from_secs(1),
            cost_evidence: "/fixture/evidence".into(),
            cost: RuntimeCost {
                id: [2; 32],
                floor: guest,
                ceiling: CapacityResources {
                    cpu_milli: 2_000,
                    mem_bytes: 128 << 20,
                    disk_bytes: 0,
                },
                quantum: CapacityResources {
                    cpu_milli: 1_000,
                    mem_bytes: 1 << 20,
                    disk_bytes: 1,
                },
                guest_overhead: CapacityResources {
                    mem_bytes: 3 << 20,
                    ..CapacityResources::default()
                },
                overhead: CapacityResources {
                    cpu_milli: 100,
                    mem_bytes: 5 << 20,
                    disk_bytes: 0,
                },
            },
            stage_limits: hex_core::stream::files::Limits {
                entries: 10,
                bytes: 1 << 20,
                path: 100,
            },
            vbios: Vec::new(),
        }
    }

    fn family_fixture() -> (FirecrackerRuntime, Arc<dyn Books>) {
        let catalog = crate::devices::Catalog::parse(b"hex-devices-1\ngpu\t256\tconfig-a\t777\tvfio\t0000:01:00.0\t7\ngpu\t512\tconfig-b\t888\tvfio\t0000:02:00.0\t8\n").unwrap();
        let owner: Arc<dyn Books> = Arc::new(super::super::handoff_tests::NoOwnerCalls);
        let runtime = FirecrackerRuntime {
            config: config(),
            catalog: triomphe::Arc::try_new(catalog).unwrap(),
            books: Arc::downgrade(&owner),
            initialized: std::sync::OnceLock::new(),
            host: vfio::Host::system().unwrap(),
            marks: single_use::MarksAt {
                dir: PathBuf::from("/nonexistent/runtime"),
                unshared: None,
            },
            uid: 0,
            boot: None,
            joins: std::sync::Mutex::new(None),
            evented: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        };
        (runtime, owner)
    }

    fn family_row(runtime: &FirecrackerRuntime, keys: &[u64], phase: BackingPhase) -> PoolRecord {
        let devices: Vec<_> = keys
            .iter()
            .map(|key| DeviceId {
                kind: DeviceKind::Gpu,
                key: *key,
            })
            .collect();
        let generation = if keys.len() == 2 {
            3
        } else if keys.first() == Some(&256) {
            4
        } else {
            5
        };
        let cost = observer_cost(&runtime.config).unwrap();
        let record = PoolRecord {
            version: hex_core::record::RECORD_VERSION,
            key: BackingKey {
                pool: observer_configuration(&runtime.config, &devices).unwrap(),
                generation: [generation; 16],
            },
            execution: Execution::Firecracker,
            cost,
            allocation: (phase != BackingPhase::Prepared)
                .then(|| cost.allocate(CapacityResources::default()).unwrap()),
            devices,
            phase,
            init: if matches!(
                phase,
                BackingPhase::Live | BackingPhase::Draining | BackingPhase::Released
            ) {
                Some(hex_core::record::ProcId {
                    pid: 7,
                    start_ticks: 11,
                })
            } else {
                None
            },
            exit: if phase == BackingPhase::Released {
                Some(hex_api::Exit::Code(0))
            } else {
                None
            },
        };
        record.check().unwrap();
        runtime.preflight_backing(&record).unwrap();
        record
    }

    fn consuming_box(runtime: &FirecrackerRuntime, source: &PoolRecord, name: &str) -> BoxRecord {
        let workload = CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: hex_api::MIN_MEMORY,
            disk_bytes: 0,
        };
        let spec = BoxSpec {
            id: BoxId::new(name).unwrap(),
            class: ClassSpec::Firecracker(Box::new(hex_api::OciSpec {
                rootfs: hex_api::Rootfs::Dir("/fixture/payload".into()),
                init: Program::of(&["true"]),
                mounts: Vec::new(),
                hostname: None,
                network: hex_api::Network::Private,
                hosts: Vec::new(),
            })),
            bounds: hex_api::Bounds {
                cpu_max_milli: Some(1_000),
                memory_max: Some(hex_api::MIN_MEMORY),
                ..hex_api::Bounds::default()
            },
            owner: None,
            devices: vec![source.devices[0]],
            allocation: Some(runtime.config.cost.allocate(workload).unwrap()),
            route: Some(hex_api::RouteReceipt {
                key: runtime
                    .route_key(hex_api::GpuEligibility::Required)
                    .unwrap(),
                backing: Some(source.key),
            }),
            band: 0,
        };
        spec.check().unwrap();
        let mut boxed = BoxRecord::new(spec, Phase::Ready, Vec::new());
        boxed.native_scope = Some(hex_core::record::NativeScope::from_bytes(
            [if name == "first-consumer" { 17 } else { 18 }; 16],
        ));
        boxed.init = Some(hex_core::record::ProcId {
            pid: 19,
            start_ticks: 23,
        });
        boxed
    }

    /// AC-T7.4.1: one current deterministic complement, retained full-P source, and ordered ACK
    /// queue during Box-end/coalescing. This is identity/metadata modeling, not native readiness.
    #[test]
    fn family_complement_keeps_parent_and_requires_ack_before_growing_a_live_scope() {
        let (runtime, _no_calls) = family_fixture();
        let configured = runtime.observer_claims().unwrap();
        let mut records = crate::devices::Records::new();
        assert!(
            matches!(runtime.observer_proposal(&configured,&records,&[]).unwrap(),
            ObserverProposal::Prepare { devices, current: None } if devices==configured)
        );
        let parent = family_row(&runtime, &[256, 512], BackingPhase::Released);
        let boxed = consuming_box(&runtime, &parent, "selected-one");
        records.insert(boxed.spec.id.clone(), Ok(boxed));
        assert!(
            matches!(runtime.observer_proposal(&configured,&records,std::slice::from_ref(&parent)).unwrap(),
            ObserverProposal::Prepare { devices,current:None } if devices==vec![configured[1]])
        );
        let current = family_row(&runtime, &[512], BackingPhase::Live);
        let mut pools = vec![parent, current];
        assert_eq!(
            runtime.fresh_observer(&records, &pools).unwrap(),
            Some(pools[1].key)
        );
        records.clear(); // The original selected Box ended; R must grow to the full configuration.
        assert!(
            matches!(runtime.observer_proposal(&configured,&records,&pools).unwrap(),
            ObserverProposal::AwaitAck(row) if row.key==pools[0].key)
        );
        assert_eq!(runtime.fresh_observer(&records, &pools).unwrap(), None);
        pools.remove(0); // The actual Index ACK path, not the proposal, owns this metadata removal.
        assert!(
            matches!(runtime.observer_proposal(&configured,&records,&pools).unwrap(),
            ObserverProposal::Prepare { devices,current:Some(row) } if devices==configured && row.key==pools[0].key)
        );
        pools[0].phase = BackingPhase::Draining;
        assert_eq!(runtime.fresh_observer(&records, &pools).unwrap(), None);
        assert_eq!(pools[0].devices, vec![configured[1]]);
        assert!(pools[0].allocation.is_some());
        pools[0].phase = BackingPhase::Released;
        pools[0].exit = Some(hex_api::Exit::Code(0));
        assert!(matches!(
            runtime
                .observer_proposal(&configured, &records, &pools)
                .unwrap(),
            ObserverProposal::AwaitAck(_)
        ));
        pools.clear(); // A separate whole-scope release and Index ACK must have completed first.
        assert!(
            matches!(runtime.observer_proposal(&configured,&records,&pools).unwrap(),
            ObserverProposal::Prepare { devices,current:None } if devices==configured)
        );
    }

    /// AC-T7.4.1: the first cold proposal carries only canonical IDs; foreign/held devices are
    /// excluded without model/VRAM/load invention, and an old Prepared can be canceled to empty.
    #[test]
    fn family_cold_and_empty_complements_preserve_exact_identity_without_facts() {
        let (runtime, _no_calls) = family_fixture();
        let configured = runtime.observer_claims().unwrap();
        let mut records = crate::devices::Records::new();
        let source = family_row(&runtime, &[256, 512], BackingPhase::Prepared);
        let mut claimed = consuming_box(
            &runtime,
            &family_row(&runtime, &[256, 512], BackingPhase::Released),
            "native-holder",
        );
        claimed.spec.route = None;
        claimed.spec.class = ClassSpec::Oci(match claimed.spec.class {
            ClassSpec::Firecracker(oci) => oci,
            other => panic!("expected Firecracker payload in family fixture, got {other:?}"),
        });
        claimed.spec.allocation = None;
        claimed.native_scope = None;
        claimed.spec.devices = configured.clone();
        claimed.spec.check().unwrap();
        records.insert(claimed.spec.id.clone(), Ok(claimed));
        assert!(matches!(
            runtime
                .observer_proposal(&configured, &records, &[])
                .unwrap(),
            ObserverProposal::Empty
        ));
        assert!(
            matches!(runtime.observer_proposal(&configured,&records,std::slice::from_ref(&source)).unwrap(),
            ObserverProposal::Prepare { devices,current:Some(row) } if devices.is_empty() && row.key==source.key)
        );
        assert_eq!(source.phase, BackingPhase::Prepared);
        assert!(source.allocation.is_none() && source.init.is_none() && source.exit.is_none());
        assert_eq!(runtime.fresh_observer(&records, &[source]).unwrap(), None);
    }

    /// AC-T7.4.1: an unrelated profile, two unconsumed current rows, or two consumers of one
    /// source are invalid authority, not fresh fallback choices. No Books/provider call is allowed.
    #[test]
    fn family_profile_current_and_consuming_reference_ambiguity_refuses() {
        let (runtime, _no_calls) = family_fixture();
        let configured = runtime.observer_claims().unwrap();
        let mut bad = family_row(&runtime, &[256], BackingPhase::Prepared);
        bad.cost.id = [42; 32];
        assert!(
            runtime
                .observer_proposal(&configured, &crate::devices::Records::new(), &[bad])
                .is_err()
        );
        let multiple = [
            family_row(&runtime, &[256], BackingPhase::Prepared),
            family_row(&runtime, &[512], BackingPhase::Prepared),
        ];
        assert!(
            runtime
                .observer_proposal(&configured, &crate::devices::Records::new(), &multiple)
                .is_err()
        );
        let source = family_row(&runtime, &[256, 512], BackingPhase::Released);
        let mut records = crate::devices::Records::new();
        for name in ["first-consumer", "second-consumer"] {
            let boxed = consuming_box(&runtime, &source, name);
            records.insert(boxed.spec.id.clone(), Ok(boxed));
        }
        assert!(
            runtime
                .observer_proposal(&configured, &records, &[source])
                .is_err()
        );
    }

    /// Joint plan step 14 (AC-T7.4.1; ROOT, 2026-11-13: no legacy receipt runs on with a census number as its bound):
    /// an observer and a VM box its node recorded under the profile's census cost are recognized exactly, the observer's
    /// identity recomputed under its own cost, and pass the preflight to be retired at the node's start; the census-free
    /// profile's own are no census ones. Do record a family row and a consuming VM under the fixture's census-carrying
    /// cost, then take the profile census-free and record another row. Expect the first row and the VM recognized and
    /// passing the preflight, and the second row not recognized.
    #[test]
    fn w14_census_records_are_recognized_by_their_identity_under_their_own_cost() {
        let (mut runtime, _no_calls) = family_fixture();
        let old = family_row(&runtime, &[256], BackingPhase::Live);
        let consumer = consuming_box(&runtime, &old, "first-consumer");
        runtime.config.cost = runtime.config.cost.census_free();
        assert!(runtime.census_observer(&old).unwrap());
        runtime.preflight_backing(&old).unwrap();
        assert!(runtime.census_vm(&consumer));
        runtime.preflight_record(&consumer).unwrap();
        let current = family_row(&runtime, &[256], BackingPhase::Prepared);
        assert!(!runtime.census_observer(&current).unwrap());
    }

    /// AC-T7.4.1: a metadata-only cold observer's fixed charge carries the full physical set;
    /// changing config, executable or a selected identity cannot reuse its configuration key.
    #[test]
    fn cold_observer_configuration_binds_full_claims_binary_and_fixed_profile() {
        let mut config = config();
        let claims = [
            DeviceId {
                kind: DeviceKind::Gpu,
                key: 1,
            },
            DeviceId {
                kind: DeviceKind::Gpu,
                key: 2,
            },
        ];
        let original = observer_configuration(&config, &claims).unwrap();
        assert_ne!(
            original,
            observer_configuration(&config, &claims[..1]).unwrap()
        );
        config.binary_sha256 = [3; 32];
        assert_ne!(original, observer_configuration(&config, &claims).unwrap());
        config.binary_sha256 = [1; 32];
        config.cost.overhead.mem_bytes += 1 << 20;
        assert_ne!(original, observer_configuration(&config, &claims).unwrap());
        assert!(observer_configuration(&config, &[claims[1], claims[0]]).is_err());
        assert!(observer_configuration(&config, &[claims[0], claims[0]]).is_err());
        assert!(
            observer_configuration(
                &config,
                &[DeviceId {
                    kind: DeviceKind::Vfio,
                    key: 1
                }]
            )
            .is_err()
        );
    }

    /// AC-T7.4.1: the configured guest consumes its granted envelope, with one host allowance and
    /// no invented BoxSpec/row. These synthetic unit values are not measured runtime costs.
    #[test]
    fn cold_observer_machine_uses_granted_guest_envelope_and_charges_host_once() {
        let config = config();
        let cost = observer_cost(&config).unwrap();
        assert_eq!(cost.floor, cost.ceiling);
        let allocation = cost.allocate(CapacityResources::default()).unwrap();
        let value = serde_json::to_value(
            config
                .machine_guest(
                    allocation.guest().unwrap(),
                    Path::new("/fixture/vsock"),
                    &[],
                    &BoxId::new("observer").unwrap(),
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(value["machine-config"]["vcpu_count"], 1);
        assert_eq!(value["machine-config"]["mem_size_mib"], 64);
        assert_eq!(allocation.resources.cpu_milli, 1_100);
        assert_eq!(allocation.resources.mem_bytes, 69 << 20);
        let mut malformed = config;
        malformed.cost.floor.cpu_milli = 500;
        assert!(observer_cost(&malformed).is_err());
    }
}
