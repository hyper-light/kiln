//! AC-T7.3.1/AC-T7.4.1: real file-backed Driver + model device metadata + SimRuntime only.
//! No device/hardware/native effect is exercised or inferred from these fixtures.

use super::*;
use hex_api::{Capacity, CapacityResources, DeviceId, DeviceInfo, DeviceKind};
use hex_core::record::{BoxRecord, Phase};
use hex_runtime::devices::{Catalog, Records};
use hex_sim::model::{Device, GpuLoad, ModelId};

const GPU_A: DeviceId = DeviceId {
    kind: DeviceKind::Gpu,
    key: 0x100,
};
const GPU_B: DeviceId = DeviceId {
    kind: DeviceKind::Gpu,
    key: 0x200,
};
const CPU_NAMESPACE: &str = "ffeeddccbbaa99887766554433221100";

#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelCall {
    Classes,
    Capacity,
    Create {
        id: BoxId,
        claims: Vec<DeviceId>,
        outcome: Result<(), Refusal>,
    },
    Start {
        id: BoxId,
        outcome: Result<(), Refusal>,
    },
    Delete {
        id: BoxId,
        outcome: Result<(), Refusal>,
    },
    Inspect(BoxId),
    Exec(BoxId),
    Run(BoxId),
    Signal(BoxId),
    Usage(BoxId),
    List,
}

/// Claims stay in test BoxRecords; the unchanged SimRuntime receives an ordinary claim-free
/// BoxSpec. Catalog admission/holder/report logic is shared production code, never a test picker.
struct DeviceRuntime {
    counted: Counting,
    catalog: Mutex<Catalog>,
    records: Mutex<Records>,
    calls: Mutex<Vec<ModelCall>>,
}

fn catalog(ids: [DeviceId; 2]) -> Catalog {
    let mut text = "hex-devices-1\n".to_owned();
    for id in ids {
        text.push_str(&format!(
            "gpu\t{}\tfixture-gpu\t{}\tcdi\t/model/unread.cdi\tfixture.example/gpu={}\n",
            id.key,
            hex_api::MIN_MEMORY,
            id.key
        ));
    }
    Catalog::parse(text.as_bytes()).unwrap()
}

impl DeviceRuntime {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            counted: Counting::default(),
            catalog: Mutex::new(catalog([GPU_A, GPU_B])),
            records: Mutex::new(Records::new()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn record(&self, call: ModelCall) {
        let mut calls = self.calls.lock().unwrap();
        // Same finite ceiling as the parent local-message fixture; never truncate receipts.
        assert!(calls.len() < crate::records::MOST);
        calls.push(call);
    }

    fn snapshot(&self) -> Vec<ModelCall> {
        self.calls.lock().unwrap().clone()
    }

    fn claims(&self, id: &BoxId) -> Vec<DeviceId> {
        self.records
            .lock()
            .unwrap()
            .get(id)
            .unwrap()
            .as_ref()
            .unwrap()
            .spec
            .devices
            .clone()
    }

    fn report(&self) -> Vec<DeviceInfo> {
        self.capacity(hex_api::Class::Oci).unwrap().unwrap().devices
    }

    fn set_model_claims(&self, id: &BoxId, claims: Vec<DeviceId>) {
        let mut records = self.records.lock().unwrap();
        let record = records.get_mut(id).unwrap().as_mut().unwrap();
        record.spec.devices = claims;
        record.spec.check().unwrap();
    }

    fn counts(&self) -> (usize, usize, usize) {
        (
            self.counted.creates.load(Ordering::SeqCst),
            self.counted.starts.load(Ordering::SeqCst),
            self.counted.deletes.load(Ordering::SeqCst),
        )
    }
}

impl Runtime for DeviceRuntime {
    fn classes(&self) -> Vec<hex_api::Class> {
        self.record(ModelCall::Classes);
        self.counted.classes()
    }

    fn capacity(&self, _class: hex_api::Class) -> Result<Option<Capacity>, Refusal> {
        self.record(ModelCall::Capacity);
        let catalog = self.catalog.lock().unwrap();
        let records = self.records.lock().unwrap();
        let devices = catalog
            .entries()
            .iter()
            .map(|entry| entry.report(catalog.holder(entry.info.id, &records, &[])?))
            .collect::<Result<_, Refusal>>()?;
        let (demand, _) = request();
        let room = CapacityResources {
            cpu_milli: demand.need.cpu_milli * 4,
            mem_bytes: demand.need.mem_bytes * 4,
            disk_bytes: 0,
        };
        let report = Capacity {
            routes: Vec::new(),
            device_routes: Vec::new(),
            backings: Vec::new(),
            cost: None,
            id: "device-driver-model-pool".to_owned(),
            total: room,
            free: room,
            devices,
            unavailable: Vec::new(),
            learned: None,
        };
        report.check()?;
        Ok(Some(report))
    }

    fn create(&self, mut spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        let requested = spec.clone();
        let outcome = (|| {
            {
                let catalog = self.catalog.lock().unwrap();
                let records = self.records.lock().unwrap();
                catalog.prepare(&requested, &records, &[])?;
            }
            // This extraction is ONLY the explicitly selected test model. SimRuntime itself
            // correctly refuses device claims; no production backend is made to claim support.
            spec.devices.clear();
            let info = self.counted.create(spec)?;
            self.records.lock().unwrap().insert(
                requested.id.clone(),
                Ok(BoxRecord::new(requested.clone(), Phase::Ready, Vec::new())),
            );
            Ok(info)
        })();
        self.record(ModelCall::Create {
            id: requested.id,
            claims: requested.devices,
            outcome: outcome.as_ref().map(|_| ()).map_err(Clone::clone),
        });
        outcome
    }

    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        let outcome = self.counted.start(id);
        self.record(ModelCall::Start {
            id: id.clone(),
            outcome: outcome.clone(),
        });
        outcome
    }
    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.record(ModelCall::Exec(id.clone()));
        self.counted.exec(id, program, stdio)
    }
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.record(ModelCall::Run(spec.id.clone()));
        self.counted.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        self.record(ModelCall::Signal(id.clone()));
        self.counted.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.record(ModelCall::Inspect(id.clone()));
        self.counted.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.record(ModelCall::Usage(id.clone()));
        self.counted.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.record(ModelCall::List);
        self.counted.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        let outcome = self.counted.delete(id);
        if outcome.is_ok() {
            self.records.lock().unwrap().remove(id);
        }
        self.record(ModelCall::Delete {
            id: id.clone(),
            outcome: outcome.clone(),
        });
        outcome
    }
}

fn gpu_request() -> (Demand, Payload) {
    let (mut demand, mut payload) = request();
    demand.need.gpus = 1;
    payload.spec.class = ClassSpec::Oci(Box::new(OciSpec {
        rootfs: Rootfs::Dir("/model/unopened-rootfs".into()),
        init: Program::of(&["wait"]),
        mounts: Vec::new(),
        hostname: None,
        network: Network::Private,
        hosts: Vec::new(),
    }));
    payload.start = true;
    (demand, payload)
}

fn gpu_node(reordered: bool) -> NodeSpec {
    let (demand, _) = request();
    let mut node = NodeSpec::new(
        NodeId(0),
        Resources {
            cpu_milli: demand.need.cpu_milli * 4,
            mem_bytes: demand.need.mem_bytes * 4,
            gpus: 2,
            ..Resources::default()
        },
    );
    let gpu = |id: DeviceId, busy_per_mille, available| Device {
        key: Some(id.key),
        available,
        load: GpuLoad {
            busy_per_mille,
            memory_used: Some(0),
        },
        ..Device::gpu(ModelId(0), hex_api::MIN_MEMORY)
    };
    node.devices = if reordered {
        vec![gpu(GPU_B, Some(0), true), gpu(GPU_A, Some(1000), false)]
    } else {
        vec![gpu(GPU_A, Some(0), true), gpu(GPU_B, Some(1000), true)]
    };
    node
}

fn model_boot(
    dir: &Path,
    node: NodeSpec,
    rt: Arc<DeviceRuntime>,
    namespace: &str,
) -> Result<Driver, Error> {
    Driver::boot_in(
        Identity {
            namespace,
            node: NodeId(0),
        },
        &[node],
        &["fixture-gpu".to_owned()],
        Budget { owned: 2, ran: 2 },
        Pick::default(),
        dir,
        Runtimes::new([(Class::Oci, rt as Arc<dyn Runtime>)]).unwrap(),
    )
}

fn all_files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (
                path.file_name().unwrap().to_str().unwrap().to_owned(),
                std::fs::read(&path).unwrap(),
            )
        })
        .collect()
}

fn held(rt: &DeviceRuntime) -> BTreeMap<DeviceId, BoxId> {
    rt.report()
        .into_iter()
        .filter_map(|info| match info.holder {
            Some(hex_api::DeviceHolder::Box(id)) => Some((info.id, id)),
            Some(hex_api::DeviceHolder::Backing(_)) | None => None,
        })
        .collect()
}

fn create_receipts(calls: &[ModelCall]) -> Vec<(BoxId, Vec<DeviceId>)> {
    calls
        .iter()
        .filter_map(|call| match call {
            ModelCall::Create {
                id,
                claims,
                outcome: Ok(()),
            } => Some((id.clone(), claims.clone())),
            _ => None,
        })
        .collect()
}

fn cleanup(driver: &Driver, rt: &DeviceRuntime, keys: &[Key]) {
    for key in keys {
        local(driver, driver.delete(*key).unwrap());
    }
    assert!(rt.list().unwrap().is_empty());
    assert!(rt.records.lock().unwrap().is_empty());
    assert!(held(rt).is_empty());
    assert!(driver.owned().unwrap().is_empty());
    assert_eq!(driver.state.lock().unwrap().node.admitted(), [0, 0, 0, 0]);
}

/// AC-T7.3.1/AC-T7.4.1: Driver must deliver its sole durable selection to the Runtime. Rebooting
/// against swapped loads/reordered keyed inventory preserves that ID and leaves the other free.
#[test]
fn model_driver_keeps_exact_gpu_binding_across_reboot_inventory_reorder_and_load_swap() {
    let dir = tempfile::tempdir().unwrap();
    let rt = DeviceRuntime::new();
    let driver = model_boot(dir.path(), gpu_node(false), rt.clone(), TEST_NAMESPACE).unwrap();
    let (demand, payload) = gpu_request();
    let (old, step) = driver.submit(demand, payload.clone()).unwrap();
    let started = local(&driver, step);
    let old_id = driver.physical_id(old).unwrap();
    let old_claims = rt.claims(&old_id);
    let old_row = driver
        .state
        .lock()
        .unwrap()
        .rows
        .row(old)
        .unwrap()
        .to_owned();
    let old_holders = held(&rt);
    let before_reboot = rt.counts();
    drop(driver);

    *rt.catalog.lock().unwrap() = catalog([GPU_B, GPU_A]);
    let recovered = model_boot(dir.path(), gpu_node(true), rt.clone(), TEST_NAMESPACE).unwrap();
    let replay = local(&recovered, recovered.take_recovered().unwrap());
    let after_reboot = rt.counts();
    let restored_id = recovered.physical_id(old).unwrap();
    let restored_claims = rt.claims(&old_id);
    let restored_row = recovered
        .state
        .lock()
        .unwrap()
        .rows
        .row(old)
        .unwrap()
        .to_owned();
    let restored_selection = recovered
        .state
        .lock()
        .unwrap()
        .node
        .assigned_device_specs(old)
        .unwrap()
        .unwrap();
    let report_order: Vec<_> = rt.report().into_iter().map(|info| info.id).collect();
    let (fresh, step) = recovered.submit(demand, payload).unwrap();
    let fresh_started = local(&recovered, step);
    let fresh_id = recovered.physical_id(fresh).unwrap();
    let fresh_claims = rt.claims(&fresh_id);
    let both = held(&rt);
    let receipts = create_receipts(&rt.snapshot());
    cleanup(&recovered, &rt, &[old, fresh]);
    let counts = rt.counts();
    let files = all_files(dir.path());
    eprintln!(
        "model-driver-reboot expected_old={GPU_A:?} observed_old={old_claims:?}/{restored_claims:?} key={old:?} id={old_id} fresh_key={fresh:?} fresh_id={fresh_id} fresh_claims={fresh_claims:?} counts={counts:?} cleanup_boxes=0 cleanup_claims=0 hardware_effect=false"
    );

    assert!(started.notices.contains(&Notice::Started {
        key: old,
        node: NodeId(0)
    }));
    assert!(replay.notices.contains(&Notice::Started {
        key: old,
        node: NodeId(0)
    }));
    assert!(fresh_started.notices.contains(&Notice::Started {
        key: fresh,
        node: NodeId(0)
    }));
    assert_eq!(old_claims, [GPU_A]);
    assert_eq!(restored_claims, [GPU_A]);
    assert_eq!(restored_id, old_id);
    assert_eq!(old_row, restored_row);
    assert_eq!(
        restored_selection.iter().map(|d| d.key).collect::<Vec<_>>(),
        [Some(GPU_A.key)]
    );
    assert_eq!(before_reboot, (1, 1, 0));
    assert_eq!(after_reboot, before_reboot);
    assert_eq!(report_order, [GPU_B, GPU_A]);
    assert_eq!(fresh_claims, [GPU_B]);
    assert_ne!(old, fresh);
    assert_ne!(old_id, fresh_id);
    assert_eq!(old_holders, BTreeMap::from([(GPU_A, old_id.clone())]));
    assert_eq!(
        both,
        BTreeMap::from([(GPU_A, old_id.clone()), (GPU_B, fresh_id.clone())])
    );
    assert_eq!(receipts, [(old_id, vec![GPU_A]), (fresh_id, vec![GPU_B])]);
    assert_eq!(counts, (2, 2, 2));
    assert_eq!(
        files.keys().map(String::as_str).collect::<Vec<_>>(),
        ["epoch.rec"]
    );
}

/// AC-T7.3.1/AC-T7.4.1: caller-supplied physical claims have no receiver authority, even when
/// individually valid. Refusal precedes durable files/Runtime calls; legitimate GPU/CPU still run.
#[test]
fn model_driver_rejects_forged_caller_claims_before_records_and_runtime_then_progresses() {
    let dir = tempfile::tempdir().unwrap();
    let rt = DeviceRuntime::new();
    let driver = model_boot(dir.path(), gpu_node(false), rt.clone(), TEST_NAMESPACE).unwrap();
    let (demand, mut forged) = gpu_request();
    forged.spec.devices = vec![GPU_B];
    forged.spec.check().unwrap();
    let before_files = all_files(dir.path());
    let before_calls = rt.snapshot();
    let rejection = driver.submit(demand, forged);
    let after_files = all_files(dir.path());
    let after_calls = rt.snapshot();
    let empty_owned = driver.owned().unwrap().is_empty();
    let (demand, payload) = gpu_request();
    let (gpu, step) = driver.submit(demand, payload).unwrap();
    let gpu_started = local(&driver, step);
    let gpu_id = driver.physical_id(gpu).unwrap();
    let gpu_claims = rt.claims(&gpu_id);
    let (demand, payload) = request();
    let (cpu, step) = driver.submit(demand, payload).unwrap();
    let cpu_started = local(&driver, step);
    let cpu_id = driver.physical_id(cpu).unwrap();
    let mut proc = rt
        .exec(
            &cpu_id,
            Program::of(&["say", "model-cpu-progress"]),
            Stdio::default(),
        )
        .unwrap();
    let cpu_exit = proc.try_wait().unwrap();
    drop(proc);
    let receipts = create_receipts(&rt.snapshot());
    cleanup(&driver, &rt, &[gpu, cpu]);
    let counts = rt.counts();
    eprintln!(
        "model-driver-forgery expected=Invalid/no_files/no_runtime observed={rejection:?} files_unchanged={} runtime_unchanged={} gpu_key={gpu:?} gpu_id={gpu_id} claims={gpu_claims:?} cpu_key={cpu:?} cpu_id={cpu_id} cpu_exit={cpu_exit:?} counts={counts:?} cleanup_boxes=0 cleanup_claims=0 hardware_effect=false",
        before_files == after_files,
        before_calls == after_calls
    );

    assert!(matches!(
        rejection,
        Err(Error::Runtime(Refusal::Invalid {
            what: "placement",
            ..
        }))
    ));
    assert_eq!(after_files, before_files);
    assert_eq!(after_calls, before_calls);
    assert!(empty_owned);
    assert_eq!(gpu.seq, 0);
    assert_eq!(gpu_claims, [GPU_A]);
    assert!(gpu_started.notices.contains(&Notice::Started {
        key: gpu,
        node: NodeId(0)
    }));
    assert!(cpu_started.notices.contains(&Notice::Started {
        key: cpu,
        node: NodeId(0)
    }));
    assert_eq!(cpu_exit, Some(hex_api::Exit::Code(0)));
    assert_eq!(receipts, [(gpu_id, vec![GPU_A]), (cpu_id, Vec::new())]);
    assert_eq!(counts, (2, 1, 2));
    assert_eq!(all_files(dir.path()).len(), 1);
}

/// AC-T7.3.1/AC-T7.4.1: a wrong modeled Runtime holder must fail before core boot writes/restarts
/// init. Repair exact metadata, recover without another create, and retain independent CPU progress.
#[test]
fn model_driver_refuses_recovered_metadata_disagreement_before_rows_or_init_then_repairs() {
    let dir = tempfile::tempdir().unwrap();
    let cpu_dir = tempfile::tempdir().unwrap();
    let rt = DeviceRuntime::new();
    let driver = model_boot(dir.path(), gpu_node(false), rt.clone(), TEST_NAMESPACE).unwrap();
    rt.counted.fail_start.store(true, Ordering::SeqCst);
    rt.counted.fail_delete.store(true, Ordering::SeqCst);
    let (demand, payload) = gpu_request();
    let (key, step) = driver.submit(demand, payload).unwrap();
    let pending = local(&driver, step);
    let id = driver.physical_id(key).unwrap();
    let initial_state = rt.inspect(&id).unwrap().state;
    let original_claims = rt.claims(&id);
    drop(driver);
    rt.counted.fail_start.store(false, Ordering::SeqCst);
    rt.counted.fail_delete.store(false, Ordering::SeqCst);
    rt.set_model_claims(&id, vec![GPU_B]);
    let before_files = all_files(dir.path());
    let before_counts = rt.counts();
    let rejection = model_boot(dir.path(), gpu_node(false), rt.clone(), TEST_NAMESPACE);
    let typed_rejection = matches!(&rejection, Err(Error::Core(Stop::Damaged(_))));
    let after_counts = rt.counts();
    let after_files = all_files(dir.path());
    let still_created = rt.inspect(&id).unwrap().state;
    drop(rejection);

    let mut cpu_node = gpu_node(false);
    cpu_node.capacity.gpus = 0;
    cpu_node.devices.clear();
    let cpu_driver = model_boot(cpu_dir.path(), cpu_node, rt.clone(), CPU_NAMESPACE).unwrap();
    let (demand, payload) = request();
    let (cpu, step) = cpu_driver.submit(demand, payload).unwrap();
    let cpu_started = local(&cpu_driver, step);
    let cpu_id = cpu_driver.physical_id(cpu).unwrap();
    let mut proc = rt
        .exec(
            &cpu_id,
            Program::of(&["say", "independent-model-cpu"]),
            Stdio::default(),
        )
        .unwrap();
    let cpu_exit = proc.try_wait().unwrap();
    drop(proc);
    let stayed_created_during_sibling = rt.inspect(&id).unwrap().state;
    rt.set_model_claims(&id, original_claims.clone());
    let recovered = model_boot(dir.path(), gpu_node(false), rt.clone(), TEST_NAMESPACE).unwrap();
    let replay = local(&recovered, recovered.take_recovered().unwrap());
    let recovered_id = recovered.physical_id(key).unwrap();
    let recovered_claims = rt.claims(&id);
    let recovered_state = rt.inspect(&id).unwrap().state;
    let receipts = create_receipts(&rt.snapshot());
    local(&cpu_driver, cpu_driver.delete(cpu).unwrap());
    let cpu_empty = cpu_driver.owned().unwrap().is_empty();
    let cpu_charge = cpu_driver.state.lock().unwrap().node.admitted();
    cleanup(&recovered, &rt, &[key]);
    let counts = rt.counts();
    let calls = rt.snapshot();
    let start_outcomes: Vec<_> = calls
        .iter()
        .filter_map(|call| match call {
            ModelCall::Start {
                id: started,
                outcome,
            } if started == &id => Some(outcome.clone()),
            _ => None,
        })
        .collect();
    eprintln!(
        "model-driver-metadata expected=Damaged/before_rows/before_init observed_typed={typed_rejection} files_unchanged={} counts_before={before_counts:?}/after={after_counts:?} target_key={key:?} target_id={id} claims={original_claims:?}/{recovered_claims:?} state={initial_state:?}/{still_created:?}/{recovered_state:?} cpu_key={cpu:?} cpu_id={cpu_id} cpu_exit={cpu_exit:?} counts={counts:?} cleanup_boxes=0 cleanup_claims=0 hardware_effect=false",
        before_files == after_files
    );

    assert!(
        matches!(pending.notices.as_slice(),[Notice::Unknown { key:unknown,refusal:Refusal::Os {op:"test start",..},cleanup:Some(Refusal::Os {op:"test delete",..}) }] if *unknown==key)
    );
    assert!(typed_rejection);
    assert_eq!(after_files, before_files);
    assert_eq!(before_counts, (1, 1, 1));
    assert_eq!(after_counts, before_counts);
    assert_eq!(initial_state, BoxState::Created);
    assert_eq!(still_created, BoxState::Created);
    assert_eq!(stayed_created_during_sibling, BoxState::Created);
    assert_eq!(original_claims, [GPU_A]);
    assert_eq!(recovered_claims, original_claims);
    assert_eq!(recovered_id, id);
    assert_eq!(recovered_state, BoxState::Running);
    assert!(replay.notices.contains(&Notice::Started {
        key,
        node: NodeId(0)
    }));
    assert!(cpu_started.notices.contains(&Notice::Started {
        key: cpu,
        node: NodeId(0)
    }));
    assert_eq!(cpu_exit, Some(hex_api::Exit::Code(0)));
    assert!(cpu_empty);
    assert_eq!(cpu_charge, [0, 0, 0, 0]);
    assert_eq!(receipts, [(id.clone(), vec![GPU_A]), (cpu_id, Vec::new())]);
    assert!(matches!(
        start_outcomes.as_slice(),
        [
            Err(Refusal::Os {
                op: "test start",
                ..
            }),
            Ok(())
        ]
    ));
    assert_eq!(counts, (2, 2, 3));
    assert_eq!(all_files(dir.path()).len(), 1);
    assert_eq!(all_files(cpu_dir.path()).len(), 1);
}

/// AC-T7.4.1: distinct exact GPU requests from one inseparable group serialize on the same
/// production Turns keys before reading/publishing the single record authority. No native calls.
#[test]
fn concurrent_isolation_group_admission_keeps_one_creating_holder() {
    use crate::turns::Turns;
    use hex_core::patience::Patience;
    use std::sync::mpsc::sync_channel;
    let catalog = Arc::new(Catalog::parse(b"hex-devices-1\ngpu\t256\ta\t4096\tvfio\t0000:01:00.0\t7\ngpu\t512\tb\t4096\tvfio\t0000:02:00.0\t7\n").unwrap());
    let turns = Arc::new(Turns::new(
        "fixture group admission",
        "fixture physical turns",
        2,
        Patience::measuring("fixture.group.turns"),
    ));
    let records = Arc::new(Mutex::new(Records::new()));
    let made = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(AtomicUsize::new(0));
    let (holding, held) = sync_channel(1);
    let (publish, publishing) = sync_channel(1);
    let first = {
        let (catalog, turns, records, made) = (
            Arc::clone(&catalog),
            Arc::clone(&turns),
            Arc::clone(&records),
            Arc::clone(&made),
        );
        std::thread::spawn(move || {
            let keys = catalog.conflict_ids(&[GPU_A]).unwrap();
            assert_eq!(keys, vec![GPU_A, GPU_B]);
            let turn_ids = keys
                .iter()
                .map(|id| BoxId::new(&format!("device-{}-{}", id.kind.name(), id.key)).unwrap())
                .collect::<Vec<_>>();
            let _turns = turn_ids
                .iter()
                .map(|id| turns.take(id).unwrap())
                .collect::<Vec<_>>();
            let spec = BoxSpec {
                route: None,
                id: BoxId::new("group-first").unwrap(),
                bounds: hex_api::Bounds::default(),
                class: hex_api::ClassSpec::Process,
                owner: None,
                allocation: None,
                devices: vec![GPU_A],
                band: 0,
            };
            let snapshot = records.lock().unwrap().clone();
            catalog.prepare(&spec, &snapshot, &[]).unwrap();
            holding.send(()).unwrap();
            publishing
                .recv_timeout(hex_api::conformance::patience())
                .unwrap();
            records.lock().unwrap().insert(
                spec.id.clone(),
                Ok(BoxRecord::new(spec, Phase::Creating, Vec::new())),
            );
            made.fetch_add(1, Ordering::SeqCst);
        })
    };
    held.recv_timeout(hex_api::conformance::patience()).unwrap();
    let (attempting, attempted) = sync_channel(1);
    let second = {
        let (catalog, turns, records, made, refused) = (
            Arc::clone(&catalog),
            Arc::clone(&turns),
            Arc::clone(&records),
            Arc::clone(&made),
            Arc::clone(&refused),
        );
        std::thread::spawn(move || {
            let keys = catalog.conflict_ids(&[GPU_B]).unwrap();
            assert_eq!(keys, vec![GPU_A, GPU_B]);
            let turn_ids = keys
                .iter()
                .map(|id| BoxId::new(&format!("device-{}-{}", id.kind.name(), id.key)).unwrap())
                .collect::<Vec<_>>();
            attempting.send(()).unwrap();
            let guards = turn_ids
                .iter()
                .map(|id| turns.take(id))
                .collect::<Result<Vec<_>, _>>();
            let Ok(_guards) = guards else {
                refused.fetch_add(1, Ordering::SeqCst);
                return;
            };
            let spec = BoxSpec {
                route: None,
                id: BoxId::new("group-second").unwrap(),
                bounds: hex_api::Bounds::default(),
                class: hex_api::ClassSpec::Process,
                owner: None,
                allocation: None,
                devices: vec![GPU_B],
                band: 0,
            };
            let snapshot = records.lock().unwrap().clone();
            if catalog.prepare(&spec, &snapshot, &[]).is_err() {
                refused.fetch_add(1, Ordering::SeqCst);
                return;
            }
            records.lock().unwrap().insert(
                spec.id.clone(),
                Ok(BoxRecord::new(spec, Phase::Creating, Vec::new())),
            );
            made.fetch_add(1, Ordering::SeqCst);
        })
    };
    attempted
        .recv_timeout(hex_api::conformance::patience())
        .unwrap();
    publish.send(()).unwrap();
    first.join().unwrap();
    second.join().unwrap();
    assert_eq!(made.load(Ordering::SeqCst), 1);
    assert_eq!(refused.load(Ordering::SeqCst), 1);
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        catalog
            .holder(GPU_B, &records, &[])
            .unwrap()
            .unwrap()
            .box_id()
            .unwrap()
            .as_str(),
        "group-first"
    );
}
