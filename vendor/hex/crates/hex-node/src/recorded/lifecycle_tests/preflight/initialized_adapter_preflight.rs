//! T-T7.1.2: retained configuration/metadata validation finishes across siblings before recovery.
//! Real Store/private files; the counted backend is SimRuntime, never a native HCS or VM oracle.
use super::*;

struct MetadataRecovery {
    backend: CountedRecovery,
    metadata: std::path::PathBuf,
    checked: Arc<AtomicUsize>,
    initialized: Arc<AtomicUsize>,
    catalog: Option<hex_runtime::devices::SharedCatalog>,
}

impl Runtime for MetadataRecovery {
    fn classes(&self) -> Vec<Class> {
        self.backend.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.backend.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.backend.start(id)
    }
    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.backend.exec(id, program, stdio)
    }
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.backend.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        self.backend.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.backend.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.backend.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.backend.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.backend.delete(id)
    }
}

impl Recover for MetadataRecovery {
    fn device_catalog(&self) -> Result<Option<hex_runtime::devices::SharedCatalog>, Refusal> {
        Ok(self.catalog.clone())
    }
    fn preflight_record(&self, record: &BoxRecord) -> Result<(), Refusal> {
        self.checked.fetch_add(1, Ordering::SeqCst);
        let bytes = std::fs::read(self.metadata.join(record.spec.id.as_str())).unwrap();
        if bytes != b"same initialized metadata" {
            return Err(NativeRecord::Box(record).refusal(hex_api::LifecycleCause::Uncertain));
        }
        Ok(())
    }
    fn preflight_pool(&self, _: &PoolRecord) -> Result<(), Refusal> {
        // Structural conflicts are checked before this adapter is reached.
        Ok(())
    }
    // Its metadata names join the complete retained cohort, as a native adapter's markers do.
    fn preflight_retained(
        &self,
        records: &hex_runtime::devices::Records,
        _: &[PoolRecord],
    ) -> Result<(), Refusal> {
        for entry in std::fs::read_dir(&self.metadata).unwrap() {
            let id = BoxId::new(entry.unwrap().file_name().to_str().unwrap()).unwrap();
            if !matches!(records.get(&id), Some(Ok(_))) {
                return Err(uncertain(&id));
            }
        }
        Ok(())
    }
    fn initialize_native(&self) -> Result<(), Refusal> {
        self.initialized.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn trace(&self, id: &BoxId) -> Result<Option<ProcId>, Refusal> {
        self.backend.trace(id)
    }
    fn alive(&self, spec: &BoxSpec) -> Result<Option<bool>, Refusal> {
        self.backend.alive(spec)
    }
    fn adopt(&self, spec: &BoxSpec, trace: Option<ProcId>) -> Result<(), Refusal> {
        self.backend.adopt(spec, trace)
    }
    fn end(&self, trace: Option<ProcId>) -> Result<(), Refusal> {
        self.backend.end(trace)
    }
}

fn node(
    root: &Path,
    catalog: Option<hex_runtime::devices::SharedCatalog>,
) -> (
    Recorded<MetadataRecovery>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let (gate, _, _) = fixture(0);
    let effects = Arc::new(AtomicUsize::new(0));
    let checked = Arc::new(AtomicUsize::new(0));
    let initialized = Arc::new(AtomicUsize::new(0));
    let metadata = root.join("metadata");
    std::fs::create_dir(&metadata).unwrap();
    let node = Recorded::open(
        MetadataRecovery {
            backend: CountedRecovery {
                programs: Backend(gate),
                effects: Arc::clone(&effects),
            },
            metadata,
            checked: Arc::clone(&checked),
            initialized: Arc::clone(&initialized),
            catalog,
        },
        &root.join("state"),
    )
    .unwrap();
    (node, effects, checked, initialized)
}

fn add(node: &Recorded<MetadataRecovery>, name: &str, bytes: &[u8]) -> BoxRecord {
    let mut cpu = spec();
    cpu.id = BoxId::new(name).unwrap();
    let record = BoxRecord::new(cpu, Phase::Ready, Vec::new());
    node.books.store.put(&record).unwrap();
    std::fs::write(node.runtime.metadata.join(name), bytes).unwrap();
    record
}

#[test]
fn later_initialized_metadata_mismatch_precedes_all_recovery_and_native_initialization() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, checked, initialized) = node(dir.path(), None);
    add(&node, "aa-cpu", b"same initialized metadata");
    add(
        &node,
        "zz-native-marker",
        b"different retained scope or binding",
    );
    let before = node.books.store.all().unwrap();
    assert!(node.recover().is_err());
    assert_eq!(checked.load(Ordering::SeqCst), 2);
    assert_eq!(initialized.load(Ordering::SeqCst), 0);
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(node.books.store.all().unwrap(), before);
    assert_eq!(
        std::fs::read(node.runtime.metadata.join("zz-native-marker")).unwrap(),
        b"different retained scope or binding"
    );
}

#[test]
fn all_initialized_cpu_metadata_passes_before_the_first_effect() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, checked, initialized) = node(dir.path(), None);
    add(&node, "aa-cpu", b"same initialized metadata");
    add(&node, "zz-cpu", b"same initialized metadata");
    let before = node.books.store.all().unwrap();
    let summary = node.recover().unwrap();
    assert_eq!(summary.adopted.len(), 2);
    assert_eq!(checked.load(Ordering::SeqCst), 2);
    assert_eq!(initialized.load(Ordering::SeqCst), 1);
    assert_eq!(effects.load(Ordering::SeqCst), 4);
    assert_eq!(node.books.store.all().unwrap(), before);
}

/// AC-T7.4.1 / T-T7.1.2: recover with a native metadata name that no retained box has; expect the
/// host's own cross-check to refuse it after every retained owner's check, before native
/// initialization or any recovery effect, keeping the record and the metadata.
#[test]
fn native_metadata_no_retained_box_names_is_refused_before_initialization() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, checked, initialized) = node(dir.path(), None);
    add(&node, "aa-cpu", b"same initialized metadata");
    let orphan = node.runtime.metadata.join("zz-orphan");
    std::fs::write(&orphan, b"same initialized metadata").unwrap();
    let before = node.books.store.all().unwrap();
    assert_eq!(
        node.recover(),
        Err(Refusal::Lifecycle {
            id: BoxId::new("zz-orphan").unwrap(),
            cause: hex_api::LifecycleCause::Uncertain,
        })
    );
    assert_eq!(checked.load(Ordering::SeqCst), 1);
    assert_eq!(initialized.load(Ordering::SeqCst), 0);
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(node.books.store.all().unwrap(), before);
    assert_eq!(std::fs::read(orphan).unwrap(), b"same initialized metadata");
}

#[test]
fn duplicate_retained_scope_is_refused_before_any_adapter_or_backend_is_entered() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, checked, initialized) = node(dir.path(), None);
    let scope = hex_core::record::NativeScope::from_bytes([0x29; 16]);
    for name in ["aa-cpu", "zz-cpu"] {
        let mut cpu = spec();
        cpu.id = BoxId::new(name).unwrap();
        let mut record = BoxRecord::new(cpu, Phase::Ready, Vec::new());
        record.native_scope = Some(scope);
        node.books.store.put(&record).unwrap();
        std::fs::write(
            node.runtime.metadata.join(name),
            b"same initialized metadata",
        )
        .unwrap();
    }
    let before = node.books.store.all().unwrap();
    assert!(node.recover().is_err());
    assert_eq!(checked.load(Ordering::SeqCst), 0);
    assert_eq!(initialized.load(Ordering::SeqCst), 0);
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(node.books.store.all().unwrap(), before);
}

#[test]
fn conflicting_box_claims_use_the_same_catalog_authority_before_any_recovery_effect() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = hex_runtime::devices::SharedCatalog::try_new(
        hex_runtime::devices::Catalog::parse(
            b"hex-devices-1\ngpu\t256\tfixture-gpu\t4096\thcs\tPCIROOT(0)#PCI(0100)\t\n",
        )
        .unwrap(),
    )
    .unwrap();
    let (node, effects, checked, initialized) = node(dir.path(), Some(catalog));
    for name in ["aa-cpu", "zz-cpu"] {
        let mut record = add(&node, name, b"same initialized metadata");
        record.spec.devices.push(hex_api::DeviceId {
            kind: hex_api::DeviceKind::Gpu,
            key: 256,
        });
        node.books.store.put(&record).unwrap();
    }
    let before = node.books.store.all().unwrap();
    assert!(node.recover().is_err());
    assert_eq!(checked.load(Ordering::SeqCst), 0);
    assert_eq!(initialized.load(Ordering::SeqCst), 0);
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(node.books.store.all().unwrap(), before);
}

#[test]
fn a_pool_and_box_cannot_both_claim_a_device_without_the_canonical_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = hex_runtime::devices::SharedCatalog::try_new(
        hex_runtime::devices::Catalog::parse(
            b"hex-devices-1\ngpu\t256\tfixture-gpu\t4096\thcs\tPCIROOT(0)#PCI(0100)\t\n",
        )
        .unwrap(),
    )
    .unwrap();
    let (node, effects, checked, initialized) = node(dir.path(), Some(catalog));
    let mut claimed = add(&node, "aa-box", b"same initialized metadata");
    let device = hex_api::DeviceId {
        kind: hex_api::DeviceKind::Gpu,
        key: 256,
    };
    claimed.spec.devices.push(device);
    node.books.store.put(&claimed).unwrap();
    // Synthetic pure profile only; no runtime measurement or resource allowance is inferred.
    let fixed = hex_api::CapacityResources {
        cpu_milli: 1000,
        mem_bytes: hex_api::MIN_MEMORY,
        disk_bytes: 0,
    };
    let cost = hex_api::RuntimeCost {
        id: [7; 32],
        floor: fixed,
        ceiling: fixed,
        quantum: hex_api::CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: hex_api::CapacityResources::default(),
        overhead: hex_api::CapacityResources::default(),
    };
    let pool = PoolRecord {
        version: hex_core::record::RECORD_VERSION,
        key: hex_api::BackingKey {
            pool: [9; 32],
            generation: [8; 16],
        },
        execution: hex_api::Execution::Firecracker,
        cost,
        allocation: Some(
            cost.allocate(hex_api::CapacityResources::default())
                .unwrap(),
        ),
        devices: vec![device],
        phase: hex_api::BackingPhase::Granted,
        init: None,
        exit: None,
    };
    let pools = node.books.store.directory().join(".pools");
    std::fs::create_dir(&pools).unwrap();
    let path = pools.join(format!("{}.json", "09".repeat(32)));
    let bytes = pool.encode().unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let before = node.books.store.all().unwrap();
    assert!(node.recover().is_err());
    assert_eq!(checked.load(Ordering::SeqCst), 0);
    assert_eq!(initialized.load(Ordering::SeqCst), 0);
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(node.books.store.all().unwrap(), before);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(node.books.store.all_pools().unwrap(), vec![pool]);
}

#[test]
fn a_route_to_an_unknown_backing_never_reaches_an_initialized_adapter() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, checked, initialized) = node(dir.path(), None);
    let mut inner = spec();
    inner.id = BoxId::new("zz-shared-inner").unwrap();
    inner.class = ClassSpec::Oci(Box::new(hex_api::OciSpec {
        rootfs: hex_api::Rootfs::Dir("/pure-fixture-root".into()),
        init: Program::of(&["/bin/true"]),
        mounts: Vec::new(),
        hostname: None,
        network: hex_api::Network::Private,
        hosts: Vec::new(),
    }));
    inner.route = Some(hex_api::RouteReceipt {
        key: hex_api::RouteKey {
            class: Class::Oci,
            image: hex_api::ImagePlatform {
                os: hex_api::ImageOs::Linux,
                arch: hex_api::ImageArch::Amd64,
            },
            execution: hex_api::Execution::ShardsShared,
            gpu: hex_api::GpuEligibility::None,
        },
        backing: Some(hex_api::BackingKey {
            pool: [3; 32],
            generation: [4; 16],
        }),
    });
    add(&node, "aa-cpu", b"same initialized metadata");
    // Store::put refuses a reference to an unknown backing (check_box_backing). A retained row can
    // still name one (an older writer, a damaged file), so it is planted as its encoded bytes.
    let dangling = BoxRecord::new(inner, Phase::Ready, Vec::new());
    std::fs::write(
        crate::store::runtime_record(
            &dir.path().join("state").join("@records"),
            &dangling.spec.id,
        ),
        dangling.encode().unwrap(),
    )
    .unwrap();
    let before = node.books.store.all().unwrap();
    assert!(before.contains_key(&dangling.spec.id));
    assert!(node.recover().is_err());
    assert_eq!(checked.load(Ordering::SeqCst), 0);
    assert_eq!(initialized.load(Ordering::SeqCst), 0);
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(node.books.store.all().unwrap(), before);
}
