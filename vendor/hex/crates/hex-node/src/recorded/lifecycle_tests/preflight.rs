//! AC-T7.4.1 / T-T7.1.2: whole retained-owner preflight precedes every backend operation.
//! It reuses its parent's canonical lifecycle fixture.
//! It drives the real Recorded/Store implementation and private files; backend effects are counted
//! SimRuntime controls, not native HCS/VM proof.
use super::*;

struct CountedRecovery {
    programs: Backend,
    effects: Arc<AtomicUsize>,
}
impl Runtime for CountedRecovery {
    fn classes(&self) -> Vec<Class> {
        self.programs.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.programs.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.programs.start(id)
    }
    fn exec(&self, id: &BoxId, p: Program, s: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.programs.exec(id, p, s)
    }
    fn run(&self, spec: BoxSpec, p: Program, s: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.programs.run(spec, p, s)
    }
    fn signal(&self, id: &BoxId, s: Signal) -> Result<(), Refusal> {
        self.programs.signal(id, s)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.programs.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.programs.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.programs.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.programs.delete(id)
    }
}
impl Recover for CountedRecovery {
    fn preflight_retained(
        &self,
        records: &hex_runtime::devices::Records,
        pools: &[PoolRecord],
    ) -> Result<(), Refusal> {
        self.programs.preflight_retained(records, pools)
    }
    fn trace(&self, _: &BoxId) -> Result<Option<ProcId>, Refusal> {
        Ok(None)
    }
    fn alive(&self, _: &BoxSpec) -> Result<Option<bool>, Refusal> {
        self.effects.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    fn adopt(&self, _: &BoxSpec, _: Option<ProcId>) -> Result<(), Refusal> {
        self.effects.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn end(&self, _: Option<ProcId>) -> Result<(), Refusal> {
        self.effects.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn retained(state: &Path) -> (Recorded<CountedRecovery>, Arc<AtomicUsize>, BoxId) {
    let (gate, _, _) = fixture(0);
    let effects = Arc::new(AtomicUsize::new(0));
    let runtime = CountedRecovery {
        programs: Backend(gate),
        effects: Arc::clone(&effects),
    };
    let node = Recorded::open(runtime, state).unwrap();
    let mut ready = spec();
    ready.id = BoxId::new("aa-retained-ready").unwrap();
    node.books
        .store
        .put(&BoxRecord::new(ready.clone(), Phase::Ready, Vec::new()))
        .unwrap();
    (node, effects, ready.id)
}

#[test]
fn retained_malformed_pool_prevents_any_backend_recovery_effect() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, id) = retained(dir.path());
    let pools = dir.path().join(".pools");
    std::fs::create_dir_all(&pools).unwrap();
    let path = pools.join(format!("{}.json", "31".repeat(32)));
    let original = b"{unreadable retained pool\n";
    std::fs::write(&path, original).unwrap();
    let outcome = node.recover();
    assert!(
        outcome.is_err(),
        "unknown retained pool cannot be ignored: {outcome:?}"
    );
    assert_eq!(
        effects.load(Ordering::SeqCst),
        0,
        "preflight precedes all native-backend callbacks"
    );
    assert!(node.books.store.get(&id).unwrap().is_some());
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[test]
fn retained_later_malformed_box_prevents_earlier_backend_probe() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, id) = retained(dir.path());
    let path = dir.path().join("zz-retained-unknown.json");
    let original = b"{unreadable retained box\n";
    std::fs::write(&path, original).unwrap();
    let outcome = node.recover();
    assert!(
        outcome.is_err(),
        "malformed retained owner is uncertain: {outcome:?}"
    );
    assert_eq!(
        effects.load(Ordering::SeqCst),
        0,
        "global validation cannot be interleaved with native recovery"
    );
    assert!(node.books.store.get(&id).unwrap().is_some());
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[test]
fn retained_valid_cpu_siblings_recover_after_the_complete_preflight() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, first) = retained(dir.path());
    let mut sibling = spec();
    sibling.id = BoxId::new("ab-retained-ready").unwrap();
    let second = sibling.id.clone();
    node.books
        .store
        .put(&BoxRecord::new(sibling, Phase::Ready, Vec::new()))
        .unwrap();
    let before = node.books.store.all().unwrap();
    let summary = node.recover().unwrap();
    assert!(summary.removed.is_empty());
    assert_eq!(summary.adopted.len(), 2);
    assert!(summary.adopted.contains(&first) && summary.adopted.contains(&second));
    assert_eq!(
        effects.load(Ordering::SeqCst),
        4,
        "one alive +one adopt per CPU sibling"
    );
    assert_eq!(node.books.store.all().unwrap(), before);
}

#[test]
fn failed_preflight_preserves_unpublished_temporary_bytes_too() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, id) = retained(dir.path());
    let temporary = dir.path().join(format!(".{id}.tmp"));
    let bytes = b"unpublished retained bytes";
    std::fs::write(&temporary, bytes).unwrap();
    std::fs::write(
        dir.path().join("zz-retained-unknown.json"),
        b"{bad retained sibling\n",
    )
    .unwrap();
    assert!(node.recover().is_err());
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(temporary).unwrap(), bytes);
}

#[test]
fn unknown_retained_json_identity_cannot_disappear_from_preflight() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, _) = retained(dir.path());
    let path = dir.path().join("@unidentified.json");
    let bytes = b"unknown cleanup identity\n";
    std::fs::write(&path, bytes).unwrap();
    assert!(node.recover().is_err());
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[test]
fn duplicate_shadow_record_cannot_hide_an_unreadable_retained_claim() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, id) = retained(dir.path());
    let path = dir.path().join(format!("{id}.json"));
    let bytes = b"{unknown older durable owner\n";
    std::fs::write(&path, bytes).unwrap();
    assert!(node.recover().is_err());
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert!(
        node.books.store.get(&id).unwrap().unwrap().is_ok(),
        "the valid runtime row was also preserved"
    );
}

#[test]
fn unknown_pool_json_identity_prevents_native_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, _) = retained(dir.path());
    let directory = dir.path().join(".pools");
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("@unknown.json");
    let bytes = b"unknown retained pool authority\n";
    std::fs::write(&path, bytes).unwrap();
    assert!(node.recover().is_err());
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[test]
fn forged_retained_physical_claim_is_not_a_free_cpu_sibling() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, id) = retained(dir.path());
    let mut record = node.books.store.get(&id).unwrap().unwrap().unwrap();
    record.spec.devices.push(hex_api::DeviceId {
        kind: hex_api::DeviceKind::Gpu,
        key: 113,
    });
    let path = crate::store::runtime_record(&dir.path().join("@records"), &id);
    let bytes = record.encode().unwrap();
    std::fs::write(&path, &bytes).unwrap();
    assert!(node.recover().is_err());
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

/// T-T7.1.2: a retained hex filename must be read at its actual spelling on a case-sensitive host.
/// This is a source hypothesis control against unchanged e52 production, not an asserted native red.
#[test]
fn uppercase_retained_pool_body_cannot_be_ignored_before_cpu_adoption() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, id) = retained(dir.path());
    let directory = dir.path().join(".pools");
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join(format!("{}.json", "A1".repeat(32)));
    let bytes = b"{unreadable uppercase retained pool\n";
    std::fs::write(&path, bytes).unwrap();
    let owner_before = node.books.store.get(&id).unwrap();
    let outcome = node.recover();
    assert!(
        outcome.is_err(),
        "all retained rows must be read: {outcome:?}"
    );
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(node.books.store.get(&id).unwrap(), owner_before);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

/// T-T7.1.2: a record in the state directory that can't be read is damage, never a crash's, as every write
/// there is whole and synced. Do: recover beside one. Expect: refused before any backend effect, the refusal
/// naming the file and what to do about it, and the file kept.
#[test]
fn t_t7_1_2_a_damaged_state_record_refuses_naming_its_file_and_the_remedy() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, _) = retained(dir.path());
    let path = dir.path().join("zz-damaged.json");
    let bytes = b"{damaged durable record\n";
    std::fs::write(&path, bytes).unwrap();
    let refused = node.recover().unwrap_err().to_string();
    assert!(refused.contains(&path.display().to_string()), "{refused}");
    assert!(refused.contains("move the file aside"), "{refused}");
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

/// T-T7.1.2: a record torn in the runtime record directory beside no guest. Only the host can hold its box,
/// and no name of a host box outlives the node to prove it ended. Do: recover. Expect: the node starts, its
/// readable sibling taken back, and the torn record kept, its name refused as `Exists`.
#[test]
fn t_t7_1_2_a_torn_record_beside_no_guest_is_kept_and_the_node_starts() {
    let dir = tempfile::tempdir().unwrap();
    let (node, _, sibling) = retained(dir.path());
    let torn = BoxId::new("ab-torn").unwrap();
    let path = crate::store::runtime_record(&dir.path().join("@records"), &torn);
    std::fs::write(&path, b"{\"version\":").unwrap();
    let recovered = node
        .recover()
        .expect("the node starts beside a torn record");
    assert_eq!(recovered.adopted, [sibling]);
    assert!(path.exists(), "a record nothing proved ended was removed");
    assert_eq!(recovered.held.len(), 1, "{:?}", recovered.held);
    assert_eq!(recovered.held[0].0, torn);
    let mut again = spec();
    again.id = torn.clone();
    assert_eq!(node.create(again), Err(Refusal::Exists(torn)));
}

/// A boot session no host is in: a UUID's form, all zeros. Every boot's is random (Linux's
/// `random/boot_id`, macOS's `kern.bootsessionuuid`), so it stands for an earlier boot's.
const EARLIER_BOOT: &str = "00000000-0000-0000-0000-000000000000";

/// T-T7.1.2: a record torn in the runtime record directory before the host's latest boot, as its node's
/// boot session marker says (a power loss tore it, and ended everything it named). Do: recover. Expect: the
/// node starts, the torn record removed as an earlier boot's, said so, none held, its readable sibling kept.
#[test]
fn t_t7_1_2_a_torn_record_from_an_earlier_boot_is_removed_as_ended() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("@boot"), format!("{EARLIER_BOOT}\n")).unwrap();
    let torn = BoxId::new("ab-torn").unwrap();
    let path = crate::store::runtime_record(&dir.path().join("@records"), &torn);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{\"version\":").unwrap();
    let (node, _, sibling) = retained(dir.path());
    let recovered = node
        .recover()
        .expect("the node starts beside a torn record");
    assert!(!path.exists(), "the earlier boot's torn record is left");
    assert!(recovered.held.is_empty(), "{:?}", recovered.held);
    assert_eq!(recovered.removed.len(), 1, "{:?}", recovered.removed);
    assert_eq!(recovered.removed[0].0, torn);
    assert!(
        recovered.removed[0].1.contains("earlier boot"),
        "{:?}",
        recovered.removed
    );
    assert_eq!(recovered.adopted, [sibling]);
}

/// T-T7.1.2: a record torn in this boot of the host, as its node's marker says: nothing tells its box ended.
/// Do: recover. Expect: the record held, as a torn one is.
#[test]
fn t_t7_1_2_a_torn_record_of_this_boot_is_held() {
    let Ok(Some(now)) = crate::store::host_boot().clone() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("@boot"), format!("{}\n", now.as_str())).unwrap();
    let torn = BoxId::new("ab-torn").unwrap();
    let path = crate::store::runtime_record(&dir.path().join("@records"), &torn);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{\"version\":").unwrap();
    let (node, _, _) = retained(dir.path());
    let recovered = node
        .recover()
        .expect("the node starts beside a torn record");
    assert!(
        path.exists(),
        "a record of this boot nothing proved ended was removed"
    );
    assert_eq!(recovered.held.len(), 1, "{:?}", recovered.held);
    assert_eq!(recovered.held[0].0, torn);
}

/// T-T7.1.2: a whole record written in an earlier boot of the host: that boot's end ended everything of
/// its box. Do: recover. Expect: the box removed as an earlier boot's, said so, never taken back; the
/// record gone and nothing of it ended (no process of it outlived its boot).
#[test]
fn t_t7_1_2_a_record_from_an_earlier_boot_is_removed_never_taken_back() {
    let dir = tempfile::tempdir().unwrap();
    let (node, effects, sibling) = retained(dir.path());
    let mut earlier = spec();
    earlier.id = BoxId::new("ab-earlier").unwrap();
    let id = earlier.id.clone();
    let mut bytes = BoxRecord::new(earlier, Phase::Ready, Vec::new())
        .encode()
        .unwrap();
    // The record as a node writes it in a boot: its session beside the rest.
    let text = String::from_utf8(bytes.clone()).unwrap();
    let body = text.trim_end().strip_suffix('}').unwrap();
    bytes = format!("{body},\"boot\":\"{EARLIER_BOOT}\"}}\n").into_bytes();
    let path = crate::store::runtime_record(&dir.path().join("@records"), &id);
    std::fs::write(&path, &bytes).unwrap();
    let before = effects.load(Ordering::SeqCst);
    let recovered = node.recover().expect("the node starts");
    assert!(!path.exists(), "the earlier boot's record is left");
    assert_eq!(recovered.adopted, [sibling]);
    assert_eq!(recovered.removed.len(), 1, "{:?}", recovered.removed);
    assert_eq!(recovered.removed[0].0, id);
    assert!(
        recovered.removed[0].1.contains("earlier boot"),
        "{:?}",
        recovered.removed
    );
    // One alive and one adopt for the sibling; nothing asked or ended of the earlier boot's box.
    assert_eq!(effects.load(Ordering::SeqCst) - before, 2);
}

/// Leaves the retained native-metadata cross-check to `Recover`'s default; its native
/// initialization counts as an effect.
struct Unchecked(CountedRecovery);
impl Runtime for Unchecked {
    fn classes(&self) -> Vec<Class> {
        self.0.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.0.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.0.start(id)
    }
    fn exec(&self, id: &BoxId, p: Program, s: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.0.exec(id, p, s)
    }
    fn run(&self, spec: BoxSpec, p: Program, s: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.0.run(spec, p, s)
    }
    fn signal(&self, id: &BoxId, s: Signal) -> Result<(), Refusal> {
        self.0.signal(id, s)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.0.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.0.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.0.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.0.delete(id)
    }
}
impl Recover for Unchecked {
    fn initialize_native(&self) -> Result<(), Refusal> {
        self.0.effects.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn trace(&self, id: &BoxId) -> Result<Option<ProcId>, Refusal> {
        self.0.trace(id)
    }
    fn alive(&self, spec: &BoxSpec) -> Result<Option<bool>, Refusal> {
        self.0.alive(spec)
    }
    fn adopt(&self, spec: &BoxSpec, trace: Option<ProcId>) -> Result<(), Refusal> {
        self.0.adopt(spec, trace)
    }
    fn end(&self, trace: Option<ProcId>) -> Result<(), Refusal> {
        self.0.end(trace)
    }
}

/// AC-T7.4.1 / T-T7.1.2: recover, preflight and initialize a node over a runtime that leaves the
/// retained native-metadata cross-check to `Recover`'s default; expect each refused, naming the
/// missing check, before native initialization or any recovery effect, and the retained box kept.
#[test]
fn a_runtime_without_its_retained_metadata_cross_check_is_refused_before_any_effect() {
    let dir = tempfile::tempdir().unwrap();
    let (gate, _, _) = fixture(0);
    let effects = Arc::new(AtomicUsize::new(0));
    let node = Recorded::open(
        Unchecked(CountedRecovery {
            programs: Backend(gate),
            effects: Arc::clone(&effects),
        }),
        dir.path(),
    )
    .unwrap();
    let mut ready = spec();
    ready.id = BoxId::new("aa-retained-ready").unwrap();
    node.books
        .store
        .put(&BoxRecord::new(ready, Phase::Ready, Vec::new()))
        .unwrap();
    let before = node.books.store.all().unwrap();
    let missing = Refusal::Broken(
        "runtime has no cross-check of its native metadata against the retained cohort".into(),
    );
    assert_eq!(node.recover(), Err(missing.clone()));
    assert_eq!(node.preflight_recovery(), Err(missing.clone()));
    assert_eq!(node.initialize_native(), Err(missing));
    assert_eq!(
        effects.load(Ordering::SeqCst),
        0,
        "the refusal precedes native initialization and every recovery effect"
    );
    assert_eq!(node.books.store.all().unwrap(), before);
}

mod initialized_adapter_preflight;
