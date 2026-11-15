//! Read-only retained native metadata, before the keeper or any recovery cleanup is entered.
use std::path::Path;

use hex_api::{BoxId, Class, ClassSpec, Refusal};
use hex_core::hcs;
use hex_core::record::{BoxRecord, NativeRecord, NativeStage, OwnerRef, PoolRecord};

use super::{Held, WinRuntime, allocation, keeper, owner_index};

impl WinRuntime {
    /// Validate the exact retained HCS mapping without HCS calls, ownership changes or publication.
    pub fn preflight_record(&self, record: &BoxRecord) -> Result<(), Refusal> {
        let spec = &record.spec;
        self.check_native_spec(spec)?;
        let bindings = self.bindings(spec)?;
        let marker = self.marker(&spec.id)?;
        let native = super::oci::read_marker_if_present(&marker)?;
        if matches!(spec.class, ClassSpec::Process) {
            if native.is_some() || record.native_scope.is_some() {
                return Err(NativeRecord::Box(record)
                    .uncertain("a host process's record holds a native marker or scope"));
            }
            return self.preflight_legacy_keeper(&spec.id);
        }
        // This reads the existing immutable initialized feature result; it never retries WMI.
        let features = self.features_off.get().ok_or_else(|| {
            invalid("the native feature configuration was not initialized before preflight")
        })?;
        if features.contains(&hex_core::setup::WindowsFeature::Containers) {
            return Err(invalid("the retained HCS backend is not initialized"));
        }
        let container = match &spec.class {
            ClassSpec::Oci(oci) => hcs::from_spec(
                oci,
                &super::oci::scratch_folder(&self.scratch_root, &spec.id)?,
            )?,
            ClassSpec::Bundle(path) => hcs::from_bundle(&super::oci::bundle_config(path)?)?,
            ClassSpec::Process
            | ClassSpec::Firecracker(_)
            | ClassSpec::Shards(_)
            | ClassSpec::ShardsImage(_) => {
                return Err(NativeRecord::Box(record)
                    .uncertain("its record's class has no retained HCS mapping"));
            }
        };
        // Missing metadata is not evidence that an asynchronous HCS create was never submitted.
        let native = native.ok_or_else(|| {
            NativeRecord::Box(record).uncertain("its retained HCS mapping has no native marker")
        })?;
        check_marker(
            &native,
            &spec.id,
            self.node.is_some(),
            &bindings,
            &container.scratch,
        )?;
        super::oci::service::preflight(&marker, &native)?;
        self.preflight_legacy_keeper(&spec.id)
    }

    /// Joined only with canonical source Live/Draining plus the guest's exact no-Intent proof.
    /// No native job is opened/adopted/stopped. Missing metadata is not a Released scope fact.
    pub fn preflight_unsubmitted(&self, record: &BoxRecord) -> Result<(), Refusal> {
        record.spec.check()?;
        let bad = |why: &str| NativeRecord::Box(record).uncertain(why);
        if record.spec.backend_class()? != Class::Shards
            || record.native_scope.is_none()
            || !matches!(
                record.phase,
                hex_core::record::Phase::Creating | hex_core::record::Phase::Deleting
            )
            || record.init.is_some()
            || record.run.is_some()
            || record.exit.is_some()
        {
            return Err(bad(
                "its handoff box isn't a pending Shards VM with a native scope and nothing run",
            ));
        }
        let cost = record
            .spec
            .allocation
            .ok_or_else(|| bad("its handoff box holds no allocation"))?
            .cost;
        self.check_vm_cost(&cost)?;
        {
            let boxes = self.boxes()?;
            if owner_index(&boxes, OwnerRef::Box(&record.spec.id)).is_ok() {
                return Err(bad("its Windows table holds a slot of its handoff box"));
            }
        }
        if let Some(keeping) = &self.holding.keeper {
            let path = keeper::vm::path(&keeping.state, OwnerRef::Box(&record.spec.id))?;
            if keeper::vm::read(&path)?.is_some() {
                return Err(bad(
                    "its Windows VM keeper holds a record of its handoff box",
                ));
            }
        }
        Ok(())
    }

    /// Read the SAME keeper commitment without opening/adopting a job or starting the keeper.
    pub fn preflight_managed(&self, record: NativeRecord<'_>) -> Result<(), Refusal> {
        let bad = |why: &str| record.uncertain(why);
        let scope = record
            .scope()
            .ok_or_else(|| bad("its record names no native scope to preflight on Windows"))?;
        if record.class()? != Class::Shards {
            return Err(bad(
                "its record isn't a Shards VM's to preflight on Windows",
            ));
        }
        let commitment = record
            .allocation()
            .map(|_| record.commitment())
            .transpose()?;
        if let Some(allocation) = record.allocation() {
            allocation.guest()?;
            self.check_vm_cost(&allocation.cost)?;
        }
        let present = {
            let boxes = self.boxes()?;
            match owner_index(&boxes, record.owner()) {
                Err(_) => false,
                Ok(at) => match boxes.get(at).and_then(|(_, slot)| slot.physical()) {
                    Some(Held::Process(boxed))
                        if commitment
                            .is_some_and(|commitment| boxed.matches_vm(record, commitment)) =>
                    {
                        true
                    }
                    _ => {
                        return Err(bad(
                            "its Windows slot isn't a VM's with its record's commitment",
                        ));
                    }
                },
            }
        };
        let Some(keeping) = self.holding.keeper.as_ref() else {
            // With no keeper there is no keeper record: the answer below for a keeper that holds none, and macOS's
            // without its helper. Prepared can have had no native effects, Released is checked later against its
            // backend's exact terminal receipt, and preflight grants no authority: no native absence witness.
            return if present
                || matches!(
                    record.stage(),
                    NativeStage::Prepared | NativeStage::Released
                ) {
                Ok(())
            } else {
                Err(bad(
                    "its node runs no Windows VM keeper, though it is past its preparation",
                ))
            };
        };
        let path = keeper::vm::path(&keeping.state, record.owner())?;
        match keeper::vm::read(&path)? {
            Some(held) if held.commits(commitment.map(|commitment| (scope, commitment))) => Ok(()),
            Some(_) => Err(bad("its Windows VM keeper's record commits another scope")),
            None if present
                || matches!(
                    record.stage(),
                    NativeStage::Prepared | NativeStage::Released
                ) =>
            {
                // Prepared cannot have native effects; Released is independently checked against
                // the exact backend terminal receipt. This is not a new native absence witness.
                Ok(())
            }
            None => Err(bad(
                "its Windows VM keeper holds no record of it, though it is past its preparation",
            )),
        }
    }

    /// Native filenames are joined against the full cohort before a sibling can be recovered.
    pub fn preflight_retained(
        &self,
        records: &crate::devices::Records,
        pools: &[PoolRecord],
    ) -> Result<(), Refusal> {
        // Same bound as the keeper service: folder, marker, next marker, request directory per box.
        let bound = crate::MAX_BOXES
            .checked_mul(4)
            .ok_or_else(|| allocation(crate::MAX_BOXES))?;
        let entries = match std::fs::read_dir(&self.scratch_root) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(Refusal::os("reading retained native marker names", error)),
        };
        if let Some(entries) = entries {
            for (count, entry) in entries.enumerate() {
                if count >= bound {
                    return Err(Refusal::Full(crate::MAX_BOXES));
                }
                let entry = entry
                    .map_err(|error| Refusal::os("reading retained native marker names", error))?;
                let name = entry.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| invalid("a native marker name is not Unicode"))?;
                let base = name
                    .strip_suffix(".next")
                    .or_else(|| name.strip_suffix(".create"))
                    .unwrap_or(name);
                if let Some(id) = base.strip_suffix(".scratch.json") {
                    let id = BoxId::new(id)?;
                    let record = records
                        .get(&id)
                        .and_then(|record| record.as_ref().ok())
                        .ok_or_else(|| {
                            invalid("native metadata has no retained canonical owner")
                        })?;
                    self.preflight_record(record)?;
                } else if Path::new(base).extension().is_some_and(|extension| {
                    extension.as_encoded_bytes().eq_ignore_ascii_case(b"json")
                }) {
                    return Err(invalid("a retained native marker name is not canonical"));
                }
            }
        }
        let Some(keeping) = self.holding.keeper.as_ref() else {
            return Ok(());
        };
        let entries = match std::fs::read_dir(&keeping.state) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Refusal::os("reading retained keeper names", error)),
        };
        // Box and Pool receipts plus one unpublished replacement each; global boxes directory,
        // keeper identity, ready file and native diagnostic. This is a work bound, never cleanup.
        let bound = crate::MAX_BOXES
            .checked_mul(4)
            .and_then(|bound| bound.checked_add(4))
            .ok_or_else(|| allocation(crate::MAX_BOXES))?;
        for (count, entry) in entries.enumerate() {
            if count >= bound {
                return Err(Refusal::Full(crate::MAX_BOXES));
            }
            let entry =
                entry.map_err(|error| Refusal::os("reading retained keeper names", error))?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| invalid("a retained keeper name is not Unicode"))?;
            if let Some(id) = name.strip_prefix("held-") {
                let id = BoxId::new(id)?;
                let record = records
                    .get(&id)
                    .and_then(|record| record.as_ref().ok())
                    .ok_or_else(|| invalid("a keeper receipt has no retained canonical box"))?;
                if record.spec.backend_class()? == Class::Shards {
                    self.preflight_managed(NativeRecord::Box(record))?;
                } else {
                    self.preflight_legacy_keeper(&id)?;
                }
            } else if name.starts_with("pool-held-") {
                let pool = keeper::vm::pool_from_name(name)?;
                let at = pools
                    .binary_search_by_key(&pool, |record| record.key.pool)
                    .map_err(|_| {
                        invalid("a VM keeper receipt has no retained canonical backing")
                    })?;
                let pool = pools.get(at).ok_or_else(|| allocation(pools.len()))?;
                if entry.path() != keeper::vm::path(&keeping.state, OwnerRef::Pool(&pool.key.pool))?
                {
                    return Err(invalid("a VM keeper receipt name is not canonical"));
                }
                self.preflight_managed(NativeRecord::Pool(pool))?;
            }
        }
        Ok(())
    }

    fn preflight_legacy_keeper(&self, id: &BoxId) -> Result<(), Refusal> {
        if let Some(keeping) = self.holding.keeper.as_ref() {
            keeper::read_held_strict(&keeping.state, id)?;
        }
        Ok(())
    }

    /// Called only after the complete Store/configuration/metadata barrier by Recorded.
    pub fn initialize_native(&self) -> Result<(), Refusal> {
        if let Some(keeping) = self.holding.keeper.as_ref() {
            self.holding.ensure_keeper(keeping)?;
        }
        Ok(())
    }
}

fn check_marker(
    record: &hcs::BoxRecord,
    id: &BoxId,
    outlives: bool,
    bindings: &[hcs::DeviceBinding],
    scratch: &hcs::Scratch,
) -> Result<(), Refusal> {
    if record.devices != bindings
        || record.outlives != outlives
        || record.system.as_ref().is_some_and(|system| system != id)
        || ((outlives || !bindings.is_empty()) && record.system.as_ref() != Some(id))
        || record
            .init
            .is_some_and(|init| init.pid == 0 || init.created == 0 || !record.started)
    {
        return Err(invalid(
            "native marker identity, lifetime or physical binding disagrees",
        ));
    }
    super::oci::validate_storage(record, scratch)?;
    if let hcs::Scratch::Folder(folder) = scratch {
        if record.folder != *folder {
            return Err(invalid(
                "native marker scratch folder disagrees with its configured owner",
            ));
        }
        let root = Path::new(folder);
        // These are the only two paths scratch() ever introduces. No canonicalization, links or
        // path prefix grants cleanup authority to another directory.
        let mut previous: Option<bool> = None;
        for made in &record.made {
            let path = Path::new(made);
            let folder = path == root;
            let sandbox = path.parent() == Some(root)
                && path.file_name().is_some_and(|name| name == "sandbox.vhdx");
            if (!folder && !sandbox) || previous.is_some_and(|was_folder| !was_folder || folder) {
                return Err(invalid(
                    "native marker cleanup paths disagree with its owned scratch",
                ));
            }
            previous = Some(folder);
        }
    }
    Ok(())
}

pub(super) fn invalid(message: &'static str) -> Refusal {
    match super::copy_text(message) {
        Ok(why) => Refusal::Invalid {
            what: "retained native preflight",
            why,
        },
        Err(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::num::NonZeroU32;
    use std::sync::{Arc, Mutex, OnceLock};

    fn runtime(root: &Path) -> WinRuntime {
        WinRuntime {
            processors: NonZeroU32::MIN,
            catalog: super::super::NativeCatalog::Empty(crate::devices::Catalog::default()),
            scratch_root: root.to_path_buf(),
            launcher: None,
            holding: Arc::new(super::super::Holding {
                boxes: Mutex::new(Vec::new()),
                keeper: None,
            }),
            node: Some("retained-preflight-fixture".into()),
            features_off: OnceLock::from(BTreeSet::new()),
            feature_reads: std::sync::atomic::AtomicU32::new(0),
            reclaim_failures: Vec::new(),
        }
    }

    fn spec() -> hex_api::BoxSpec {
        hex_api::BoxSpec {
            id: BoxId::new("owned-marker").unwrap(),
            class: ClassSpec::Oci(Box::new(hex_api::OciSpec {
                rootfs: hex_api::Rootfs::Layers(vec![r"C:\private-fixture\base".into()]),
                init: hex_api::Program::of(&["cmd.exe", "/c", "exit", "0"]),
                mounts: Vec::new(),
                hostname: None,
                network: hex_api::Network::Private,
                hosts: Vec::new(),
            })),
            bounds: hex_api::Bounds::default(),
            owner: None,
            devices: Vec::new(),
            allocation: None,
            route: None,
            band: 0,
        }
    }

    fn marker(runtime: &WinRuntime, record: &BoxRecord) -> hcs::BoxRecord {
        let folder =
            super::super::oci::scratch_folder(&runtime.scratch_root, &record.spec.id).unwrap();
        hcs::BoxRecord {
            made: vec![folder.clone()],
            folder,
            outlives: true,
            system: Some(record.spec.id.clone()),
            started: false,
            init: None,
            devices: Vec::new(),
            storage: hcs::RecordStorage::OwnedFolder,
            creation: hcs::NativeCreate::Created,
        }
    }

    /// T-T7.1.2: actual marker reads reject CPU-only foreign cleanup authority before adoption.
    /// Only private files and initialized metadata are used; zero HCS/WMI/job operations.
    #[test]
    fn retained_cpu_marker_preflight_preserves_foreign_system_and_storage_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(dir.path());
        let record = BoxRecord::new(spec(), hex_core::record::Phase::Ready, Vec::new());
        let native = marker(&runtime, &record);
        let path = runtime.marker(&record.spec.id).unwrap();
        std::fs::write(&path, hcs::write_record(&native).unwrap()).unwrap();
        runtime.preflight_record(&record).unwrap();
        for case in 0..5 {
            let mut malformed = native.clone();
            match case {
                0 => malformed.system = Some(BoxId::new("foreign-system").unwrap()),
                1 => malformed.folder = dir.path().join("foreign").to_str().unwrap().into(),
                2 => malformed
                    .made
                    .push(dir.path().join("foreign").to_str().unwrap().into()),
                3 => malformed.outlives = false,
                _ => {
                    malformed.storage = hcs::RecordStorage::BorrowedVolume;
                    malformed.made.clear();
                }
            }
            let bytes = hcs::write_record(&malformed).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            assert!(runtime.preflight_record(&record).is_err(), "case {case}");
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert!(runtime.boxes().unwrap().is_empty());
        }
        assert_eq!(
            runtime
                .feature_reads
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    /// T-T7.1.2: absent native metadata supplies no create/cancel/termination witness.
    #[test]
    fn pending_hcs_marker_absence_or_missing_service_receipt_never_initializes_native_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = runtime(dir.path());
        runtime.catalog = super::super::NativeCatalog::Shared(
            crate::devices::SharedCatalog::try_new(
                crate::devices::Catalog::parse(
                    b"hex-devices-1\ngpu\t256\tfixture\t4096\thcs\tPCIROOT(0)#PCI(0100)\t\n",
                )
                .unwrap(),
            )
            .unwrap(),
        );
        let mut assigned = spec();
        assigned.devices.push(hex_api::DeviceId {
            kind: hex_api::DeviceKind::Gpu,
            key: 256,
        });
        let record = BoxRecord::new(assigned, hex_core::record::Phase::Creating, Vec::new());
        assert!(runtime.preflight_record(&record).is_err());
        let mut native = marker(&runtime, &record);
        native.devices = runtime.bindings(&record.spec).unwrap();
        native.creation = hcs::NativeCreate::Pending;
        let bytes = hcs::write_record(&native).unwrap();
        let path = runtime.marker(&record.spec.id).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let ClassSpec::Oci(oci) = &record.spec.class else {
            panic!("the fixture is structured HCS OCI")
        };
        let container = hcs::from_spec(
            oci,
            &super::super::oci::scratch_folder(&runtime.scratch_root, &record.spec.id).unwrap(),
        )
        .unwrap();
        check_marker(
            &native,
            &record.spec.id,
            true,
            &runtime.bindings(&record.spec).unwrap(),
            &container.scratch,
        )
        .unwrap();
        assert!(
            super::super::oci::service::preflight(&path, &native).is_err(),
            "the valid pending assigned marker has no service receipt"
        );
        assert!(runtime.preflight_record(&record).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(runtime.boxes().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// T-T7.1.2: a native marker not represented by the canonical cohort cannot be swept first.
    #[test]
    fn orphan_native_marker_refuses_without_reclaiming_or_starting_a_keeper() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(dir.path());
        let record = BoxRecord::new(spec(), hex_core::record::Phase::Ready, Vec::new());
        let path = runtime.marker(&record.spec.id).unwrap();
        let bytes = hcs::write_record(&marker(&runtime, &record)).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            runtime
                .preflight_retained(&crate::devices::Records::new(), &[])
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(runtime.holding.keeper.is_none());
    }

    /// T-T7.1.2: configuration retention cannot execute a queued native request before the barrier.
    #[test]
    fn keeper_configuration_is_read_only_and_legacy_cpu_preflight_stays_positive() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("not-created");
        let keeping =
            super::super::Keeping::configured(&state, &["hex.exe".into(), "keep".into()]).unwrap();
        assert!(matches!(
            *keeping.keeper.lock().unwrap(),
            super::super::KeeperState::Configured
        ));
        assert!(!state.exists());
        let runtime = runtime(dir.path());
        let mut cpu = spec();
        cpu.class = ClassSpec::Process;
        runtime
            .preflight_record(&BoxRecord::new(
                cpu,
                hex_core::record::Phase::Ready,
                Vec::new(),
            ))
            .unwrap();
        assert!(runtime.boxes().unwrap().is_empty());
    }

    /// AC-T7.4.1: the native half of pending Shards handoff checks exact local/keeper metadata.
    /// Private files and a reserved slot only; zero HCS/job/WMI/keeper-start operations.
    #[test]
    fn unsubmitted_handoff_keeps_local_presence_and_keeper_errors_uncertain() {
        let dir = tempfile::tempdir().unwrap();
        let native = runtime(dir.path());
        let fixed = hex_api::CapacityResources {
            cpu_milli: 1000,
            mem_bytes: shards_api::min_memory_mib(std::env::consts::ARCH) << 20,
            disk_bytes: 0,
        };
        let cost = hex_api::RuntimeCost {
            id: [0x71; 32],
            floor: fixed,
            ceiling: fixed,
            quantum: hex_api::CapacityResources {
                cpu_milli: 1000,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            guest_overhead: hex_api::CapacityResources::default(),
            overhead: hex_api::CapacityResources::default(),
        };
        let mut selected = spec();
        selected.class = ClassSpec::Shards(Box::new(hex_api::OciSpec {
            rootfs: hex_api::Rootfs::Dir(dir.path().to_path_buf()),
            init: hex_api::Program::of(&["/bin/true"]),
            mounts: Vec::new(),
            hostname: None,
            network: hex_api::Network::Private,
            hosts: Vec::new(),
        }));
        selected.allocation = Some(
            cost.allocate(hex_api::CapacityResources::default())
                .unwrap(),
        );
        let mut record = BoxRecord::new(selected, hex_core::record::Phase::Creating, Vec::new());
        record.native_scope = Some(hex_core::record::NativeScope::from_bytes([0x72; 16]));
        native.preflight_unsubmitted(&record).unwrap();
        record.phase = hex_core::record::Phase::Deleting;
        native.preflight_unsubmitted(&record).unwrap();
        native
            .holding
            .reserve(OwnerRef::Box(&record.spec.id), &[], None, None)
            .unwrap();
        assert!(
            matches!(native.preflight_unsubmitted(&record), Err(Refusal::Lifecycle { id, cause: hex_api::LifecycleCause::Uncertain, .. }) if id == record.spec.id)
        );
        assert_eq!(native.boxes().unwrap().len(), 1);

        let mut kept = runtime(dir.path());
        let state = dir.path().join("unstarted-keeper");
        Arc::get_mut(&mut kept.holding).unwrap().keeper = Some(
            super::super::Keeping::configured(&state, &["must-not-execute.exe".into()]).unwrap(),
        );
        kept.preflight_unsubmitted(&record).unwrap();
        assert!(!state.exists());
        std::fs::create_dir(&state).unwrap();
        let path = keeper::vm::path(&state, OwnerRef::Box(&record.spec.id)).unwrap();
        let held = keeper::vm::Held {
            scope: record.native_scope.unwrap(),
            commitment: record.native_commitment().unwrap(),
            pid: 17,
            started: 19,
            value: 23,
        }
        .encode()
        .unwrap();
        std::fs::write(&path, &held).unwrap();
        assert!(
            kept.preflight_unsubmitted(&record).is_err(),
            "even a matching positive keeper receipt contradicts no submission"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), held);
        let malformed = b"unreadable-keeper-commitment\n";
        std::fs::write(&path, malformed).unwrap();
        assert!(kept.preflight_unsubmitted(&record).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), malformed);
        assert!(kept.boxes().unwrap().is_empty());
        assert!(matches!(
            *kept.holding.keeper.as_ref().unwrap().keeper.lock().unwrap(),
            super::super::KeeperState::Configured
        ));
        assert_eq!(
            kept.feature_reads.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    /// AC-T7.4.1: original new-feature edge. An unallocated cancellation ACK asks only about
    /// native submission metadata, including on an in-process owner configured without a keeper.
    /// This creates no native process/terminal result and opens/adopts/starts no native object.
    #[test]
    fn original_unallocated_released_without_keeper_checks_same_submission_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let native = runtime(dir.path());
        assert!(native.holding.keeper.is_none() && native.boxes().unwrap().is_empty());
        let fixed = hex_api::CapacityResources {
            cpu_milli: 1000,
            mem_bytes: shards_api::min_memory_mib(std::env::consts::ARCH) << 20,
            disk_bytes: 0,
        };
        let cost = hex_api::RuntimeCost {
            id: [0x81; 32],
            floor: fixed,
            ceiling: fixed,
            quantum: hex_api::CapacityResources {
                cpu_milli: 1000,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            guest_overhead: hex_api::CapacityResources::default(),
            overhead: hex_api::CapacityResources::default(),
        };
        native.check_vm_cost(&cost).unwrap();
        let prepared = PoolRecord {
            version: hex_core::record::RECORD_VERSION,
            key: hex_api::BackingKey {
                pool: [0x82; 32],
                generation: [0x83; 16],
            },
            execution: hex_api::Execution::ShardsShared,
            cost,
            allocation: None,
            devices: Vec::new(),
            phase: hex_api::BackingPhase::Prepared,
            init: None,
            exit: None,
        };
        prepared.check().unwrap();
        native
            .preflight_managed(NativeRecord::Pool(&prepared))
            .unwrap();
        let mut cancelled = PoolRecord::decode(&prepared.encode().unwrap()).unwrap();
        cancelled.phase = hex_api::BackingPhase::Released;
        prepared.accepts(&cancelled).unwrap(); // Legal no-grant cancellation, not ScopeReleased.
        cancelled.check().unwrap();
        assert_eq!(cancelled.key, prepared.key);
        assert_eq!(cancelled.cost, prepared.cost);
        assert_eq!(cancelled.scope(), prepared.scope());
        assert!(
            cancelled.allocation.is_none() && cancelled.init.is_none() && cancelled.exit.is_none()
        );
        let before = cancelled.encode().unwrap();
        assert!(dir.path().read_dir().unwrap().next().is_none());
        let result = native.preflight_managed(NativeRecord::Pool(&cancelled));
        assert!(native.boxes().unwrap().is_empty() && native.holding.keeper.is_none());
        assert_eq!(cancelled.encode().unwrap(), before);
        assert!(dir.path().read_dir().unwrap().next().is_none());
        assert_eq!(
            native
                .feature_reads
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        result.unwrap(); // Original oracle: the same no-submission metadata shape must remain readable.
    }
}
