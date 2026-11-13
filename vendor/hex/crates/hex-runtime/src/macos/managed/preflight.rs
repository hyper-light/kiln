//! Metadata-only checks at the SAME ProcessBox/native-owner seam.
use hex_api::{Class, Execution, ImageArch, ImageOs};
use hex_core::record::{BoxRecord, NativeStage, OwnerRef, PoolRecord};

use super::*;

impl MacRuntime {
    pub fn preflight_record(&self, record: &BoxRecord) -> Result<(), Refusal> {
        record.spec.check()?;
        let bad = || NativeRecord::Box(record).refusal(LifecycleCause::Uncertain);
        if record.spec.class.class() != Class::Process
            || !record.spec.devices.is_empty()
            || record.native_scope.is_some()
        {
            return Err(bad());
        }
        if let Some(route) = record.spec.route
            && (route.key.execution != Execution::Native
                || route.key.image.os != ImageOs::Macos
                || route.key.image.arch != ImageArch::Arm64)
        {
            return Err(bad());
        }
        let boxes = self.boxes()?;
        if let Ok(at) = native_owner_index(&boxes, OwnerRef::Box(&record.spec.id)) {
            let boxed = boxes.get(at).map(|(_, boxed)| boxed).ok_or_else(bad)?;
            if boxed.scope().is_some() {
                return Err(bad());
            }
            if let Some(init) = record.init {
                let pid = i32::try_from(init.pid).map_err(|_| bad())?;
                if boxed.group()? != Some((pid, init.start_ticks)) {
                    return Err(bad());
                }
            }
        }
        Ok(())
    }

    /// The host half of a pending dedicated handoff's no-Intent proof (AC-T7.4.1). A Box's launch,
    /// scope and receipt live only in this owner's native table and its persistent helper, so a
    /// slot of its id in either contradicts "never submitted", and an unreadable one proves
    /// nothing. Nothing is opened, adopted or stopped; absence is not a Released fact.
    pub fn preflight_unsubmitted(&self, record: &BoxRecord) -> Result<(), Refusal> {
        let native = NativeRecord::Box(record);
        let bad = || native.refusal(LifecycleCause::Uncertain);
        if !matches!(
            native.stage(),
            NativeStage::Creating | NativeStage::Deleting
        ) || native.scope().is_none()
            || record.init.is_some()
            || record.run.is_some()
            || record.exit.is_some()
        {
            return Err(bad());
        }
        // The envelope a launch of this row would take: Shards, its admitted allocation and cost,
        // checked against the measured CPU control and the configured persistent owner.
        host_bounds(native)?;
        self.check_vm_cost(&record.spec.allocation.ok_or_else(bad)?.cost)?;
        {
            let boxes = self.boxes().map_err(|_| bad())?;
            if native_owner_index(&boxes, native.owner()).is_ok() {
                return Err(bad());
            }
        }
        super::remote::preflight_unsubmitted(self, record)
    }

    pub fn preflight_managed(&self, record: NativeRecord<'_>) -> Result<(), Refusal> {
        let bad = || record.refusal(LifecycleCause::Uncertain);
        record.scope().ok_or_else(bad)?;
        if record.class()? != Class::Shards {
            return Err(bad());
        }
        if self.managed_helper.is_some() {
            return super::remote::preflight(self, record);
        }
        let boxes = self.boxes()?;
        let Ok(at) = native_owner_index(&boxes, record.owner()) else {
            return if matches!(
                record.stage(),
                NativeStage::Prepared | NativeStage::Released
            ) {
                // No scope authority is synthesized here. The backend independently validates its
                // exact no-effect/Released receipt; no native cleanup is performed by preflight.
                Ok(())
            } else {
                // Cold PGID/controller observation supplies no persistent native cleanup owner.
                // Until the same helper receipt is available, retain uncertainty BEFORE siblings.
                Err(bad())
            };
        };
        let boxed = boxes.get(at).map(|(_, boxed)| boxed).ok_or_else(bad)?;
        let scope = boxed.scope().ok_or_else(bad)?;
        if !scope.matches(record, record.commitment()?) {
            return Err(bad());
        }
        if let Some(init) = record.init()
            && boxed.identity.get().is_none_or(|identity| {
                identity.pid != init.pid || identity.started != init.start_ticks
            })
        {
            return Err(bad());
        }
        if matches!(*scope.lock().map_err(|_| bad())?, Phase::Uncertain(_)) {
            return Err(bad());
        }
        Ok(())
    }

    pub fn preflight_retained(
        &self,
        records: &crate::devices::Records,
        pools: &[PoolRecord],
    ) -> Result<(), Refusal> {
        // Capture only keys/receipts already retained by this native owner; no second native table.
        // Never recursively take its mutex from a per-owner check.
        let boxes = self.boxes()?;
        for (owner, boxed) in boxes.iter() {
            let record = match owner.as_ref() {
                OwnerRef::Box(id) => NativeRecord::Box(
                    records
                        .get(id)
                        .and_then(|record| record.as_ref().ok())
                        .ok_or_else(|| uncertain(id))?,
                ),
                OwnerRef::Pool(pool) => {
                    let generation = *boxed
                        .scope()
                        .ok_or_else(|| invalid("a retained native pool has no managed scope"))?
                        .scope
                        .bytes();
                    let at = pools
                        .binary_search_by_key(pool, |record| record.key.pool)
                        .map_err(|_| Refusal::Backing {
                            key: hex_api::BackingKey {
                                pool: *pool,
                                generation,
                            },
                            cause: LifecycleCause::Uncertain,
                        })?;
                    NativeRecord::Pool(
                        pools
                            .get(at)
                            .ok_or_else(|| native_allocation(pools.len()))?,
                    )
                }
            };
            match boxed.scope() {
                Some(scope) if scope.matches(record, record.commitment()?) => {}
                None if matches!(record, NativeRecord::Box(record) if record.spec.class.class() == Class::Process && record.native_scope.is_none()) =>
                    {}
                _ => return Err(record.refusal(LifecycleCause::Uncertain)),
            }
        }
        drop(boxes);
        super::remote::preflight_retained(self, records, pools)
    }
}

fn invalid(message: &'static str) -> Refusal {
    let mut why = String::new();
    if why.try_reserve_exact(message.len()).is_err() {
        return native_allocation(message.len());
    }
    why.push_str(message);
    Refusal::Invalid {
        what: "retained native metadata",
        why,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_api::{
        BackingKey, BackingPhase, BoxSpec, CapacityResources, ClassSpec, DeviceId, DeviceKind,
        GpuEligibility, ImagePlatform, Network, OciSpec, Program, Rootfs, RouteKey, RouteReceipt,
        RuntimeCost,
    };
    use hex_core::record::ProcId;

    /// The configured runtime a launch needs: the source's measured cost identity (the response
    /// is a test fixture; no enforcement conclusion) and its persistent helper's private paths in
    /// `dir`. Configuring either starts and reads nothing.
    fn runtime(dir: &std::path::Path) -> MacRuntime {
        std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let model = crate::macos::machine_model().unwrap();
        let rates = hex_core::watch::seed(&model);
        MacRuntime::new(
            rates.bytes,
            rates.processes,
            hex_core::watch::lateness_seed(&model),
        )
        .unwrap()
        .with_cpu_control(crate::macos::CpuControl {
            cost_id: source().cost.id,
            response: std::time::Duration::from_nanos(1),
        })
        .unwrap()
        .with_managed_helper(
            crate::macos::ManagedHelper::new(
                dir.join("hex"),
                dir.join("helper.sock"),
                dir.join("helper.receipt"),
                std::time::Duration::from_secs(1),
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn gpu() -> DeviceId {
        DeviceId {
            kind: DeviceKind::Gpu,
            key: 7,
        }
    }

    /// A still-held dedicated source with its controller's identity published.
    fn source() -> PoolRecord {
        let fixed = CapacityResources {
            cpu_milli: 1000,
            mem_bytes: shards_api::min_memory_mib(std::env::consts::ARCH) << 20,
            disk_bytes: 0,
        };
        let cost = RuntimeCost {
            id: [0x51; 32],
            floor: fixed,
            ceiling: fixed,
            quantum: CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            overhead: CapacityResources::default(),
            guest_overhead: CapacityResources::default(),
        };
        let pool = PoolRecord {
            version: hex_core::record::RECORD_VERSION,
            key: BackingKey {
                pool: [0x52; 32],
                generation: [0x53; 16],
            },
            execution: Execution::ShardsDedicated,
            cost,
            allocation: Some(cost.allocate(CapacityResources::default()).unwrap()),
            devices: vec![gpu()],
            phase: BackingPhase::Live,
            init: Some(ProcId {
                pid: 11,
                start_ticks: 13,
            }),
            exit: None,
        };
        pool.check().unwrap();
        pool
    }

    /// The Box reserved on `source`: Creating, its scope introduced, no init, run or exit.
    fn pending(root: &std::path::Path, source: &PoolRecord) -> BoxRecord {
        let spec = BoxSpec {
            id: BoxId::new("pending-handoff").unwrap(),
            class: ClassSpec::Shards(Box::new(OciSpec {
                rootfs: Rootfs::Dir(root.to_path_buf()),
                init: Program::of(&["/bin/true"]),
                mounts: Vec::new(),
                hostname: None,
                network: Network::Private,
                hosts: Vec::new(),
            })),
            bounds: Bounds {
                cpu_max_milli: Some(1000),
                memory_max: Some(hex_api::MIN_MEMORY),
                ..Bounds::default()
            },
            owner: None,
            devices: vec![gpu()],
            allocation: source.allocation,
            route: Some(RouteReceipt {
                key: RouteKey {
                    class: Class::Shards,
                    image: ImagePlatform {
                        os: ImageOs::Linux,
                        arch: ImageArch::Arm64,
                    },
                    execution: Execution::ShardsDedicated,
                    gpu: GpuEligibility::Any,
                },
                backing: Some(source.key),
            }),
            band: 0,
        };
        spec.check().unwrap();
        let mut record = BoxRecord::new(spec, hex_core::record::Phase::Creating, Vec::new());
        record.native_scope = Some(NativeScope::from_bytes([0x54; 16]));
        record
    }

    /// The first two steps of `launch` for `record`: its scope prepared and retained, before any
    /// spawn.
    fn retained(native: &MacRuntime, record: NativeRecord<'_>) -> Arc<ProcessBox> {
        let bounds = host_bounds(record).unwrap();
        let boxed = ProcessBox::prepare_managed(
            record,
            &bounds,
            &native.shared,
            native.cpu_control,
            std::time::Duration::ZERO,
        )
        .unwrap();
        retain(native, record, &boxed).unwrap();
        boxed
    }

    fn uncertain(result: Result<(), Refusal>, record: &BoxRecord) -> bool {
        matches!(result, Err(Refusal::Lifecycle { id, cause: LifecycleCause::Uncertain }) if id == record.spec.id)
    }

    /// AC-T7.4.1: with the Box's launch retained (its scope prepared, before any spawn) and then its
    /// receipt's identity, the host half refuses Uncertain and changes neither. The source's own
    /// retained slot is another owner's: alone, the Box's proof passes.
    #[test]
    fn unsubmitted_handoff_refuses_a_retained_launch_or_receipt_and_reads_only_the_box_slot() {
        let root = tempfile::tempdir().unwrap();
        let source = source();
        let record = pending(root.path(), &source);
        let native = runtime(root.path());
        native.preflight_unsubmitted(&record).unwrap();
        assert!(native.boxes().unwrap().is_empty());

        let launch = retained(&native, NativeRecord::Box(&record));
        assert!(uncertain(native.preflight_unsubmitted(&record), &record));
        launch
            .identity
            .set(Identity {
                pid: 41,
                started: 43,
                vsock: root.path().join("vsock"),
            })
            .unwrap();
        assert!(uncertain(native.preflight_unsubmitted(&record), &record));
        assert_eq!(*launch.scope().unwrap().lock().unwrap(), Phase::Prepared);
        assert_eq!(launch.group().unwrap(), None);
        assert_eq!(native.boxes().unwrap().len(), 1);

        let held = runtime(root.path());
        let _source = retained(&held, NativeRecord::Pool(&source));
        held.preflight_unsubmitted(&record).unwrap();
        assert_eq!(held.boxes().unwrap().len(), 1);
    }

    /// AC-T7.4.1: a row the host can't take for an unsubmitted reservation, or an unreadable native
    /// table, is Uncertain for the Box, never a pass.
    #[test]
    fn unsubmitted_handoff_refuses_submitted_metadata_and_an_unreadable_table() {
        let root = tempfile::tempdir().unwrap();
        let pending = pending(root.path(), &source());
        let native = runtime(root.path());
        let trace = ProcId {
            pid: 51,
            start_ticks: 53,
        };
        for change in 0..6 {
            let mut record = pending.clone();
            match change {
                0 => record.phase = hex_core::record::Phase::Ready,
                1 => record.native_scope = None,
                2 => record.init = Some(trace),
                3 => record.run = Some(trace),
                4 => record.exit = Some(hex_api::Exit::Code(0)),
                _ => {
                    record.spec.class = ClassSpec::Process;
                    record.spec.route = None;
                    record.spec.devices.clear();
                    record.spec.allocation = None;
                }
            }
            assert!(
                uncertain(native.preflight_unsubmitted(&record), &record),
                "change {change}"
            );
        }
        native.preflight_unsubmitted(&pending).unwrap();
        let table = Arc::clone(&native.boxes);
        std::thread::spawn(move || {
            let _held = table.lock().unwrap();
            panic!("poisons the native table");
        })
        .join()
        .unwrap_err();
        assert!(uncertain(native.preflight_unsubmitted(&pending), &pending));
    }
}
