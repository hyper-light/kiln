//! AC-T7.4.1 / T-T7.1.2: the public Recorded API against actual SimRuntime effects and durable files.
use super::*;
use hex_api::sim::SimRuntime;
use hex_api::{Bounds, ClassSpec};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};

struct Gate {
    entered: SyncSender<usize>,
    releases: Mutex<Vec<Receiver<()>>>,
    calls: AtomicUsize,
    fail: AtomicBool,
    sim: SimRuntime,
    forwarded: Mutex<Option<Reply>>,
    catalog: std::sync::OnceLock<hex_runtime::devices::SharedCatalog>,
}

#[derive(Clone)]
struct Backend(Arc<Gate>);

impl Runtime for Backend {
    fn classes(&self) -> Vec<Class> {
        self.0.sim.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        let n = self.0.calls.fetch_add(1, Ordering::SeqCst);
        let made = self.0.sim.create(spec)?;
        let release = {
            let mut releases = self.0.releases.lock().unwrap();
            if releases.is_empty() {
                None
            } else {
                Some(releases.remove(0))
            }
        };
        if let Some(release) = release {
            self.0.entered.send(n).unwrap();
            release.recv().unwrap();
        }
        if self.0.fail.load(Ordering::SeqCst) {
            Err(Refusal::os(
                "create fixture",
                "physical box exists but publication failed",
            ))
        } else {
            Ok(made)
        }
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.0.sim.start(id)
    }
    fn run(&self, spec: BoxSpec, p: Program, s: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.0.sim.run(spec, p, s)
    }
    fn exec(&self, id: &BoxId, p: Program, s: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.0.sim.exec(id, p, s)
    }
    fn signal(&self, id: &BoxId, s: Signal) -> Result<(), Refusal> {
        self.0.sim.signal(id, s)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.0.sim.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.0.sim.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.0.sim.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.0.sim.delete(id)
    }
}
impl Recover for Backend {
    fn forward(&self, call: Call, stdio: Stdio, reply: Reply, _ended: serve::Ended) -> Forward {
        match call {
            Call::Signal { .. } | Call::Inspect(_) => {
                *self.0.forwarded.lock().unwrap() = Some(reply);
                Forward::Taken
            }
            call => Forward::Back(Box::new((call, stdio))),
        }
    }
    // A test that claims devices sets the canonical catalog, the one physical identity authority.
    fn device_catalog(&self) -> Result<Option<hex_runtime::devices::SharedCatalog>, Refusal> {
        Ok(self.0.catalog.get().cloned())
    }
    // SimRuntime keeps no native metadata: it has no names to join to the retained cohort.
    fn preflight_retained(
        &self,
        _: &hex_runtime::devices::Records,
        _: &[PoolRecord],
    ) -> Result<(), Refusal> {
        Ok(())
    }
    fn trace(&self, _: &BoxId) -> Result<Option<ProcId>, Refusal> {
        Ok(None)
    }
    fn adopt(&self, _: &BoxSpec, _: Option<ProcId>) -> Result<(), Refusal> {
        Ok(())
    }
    fn end(&self, _: Option<ProcId>) -> Result<(), Refusal> {
        Ok(())
    }
    fn alive(&self, _: &BoxSpec) -> Result<Option<bool>, Refusal> {
        Err(Refusal::os(
            "alive fixture",
            "incomplete create has no ready-system witness",
        ))
    }
    fn discard(&self, spec: &BoxSpec) -> Result<(), Refusal> {
        match self.0.sim.delete(&spec.id) {
            Ok(()) | Err(Refusal::NoSuchBox(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

mod authority;
fn spec() -> BoxSpec {
    BoxSpec {
        route: None,
        id: BoxId::new("same-identity").unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::Process,
        owner: None,
        allocation: None,
        devices: Vec::new(),
        band: 0,
    }
}
fn fixture(pauses: usize) -> (Arc<Gate>, Receiver<usize>, Vec<SyncSender<()>>) {
    let (tx, rx) = mpsc::sync_channel(pauses);
    let (release, senders): (Vec<_>, Vec<_>) = (0..pauses)
        .map(|_| {
            let (tx, rx) = mpsc::sync_channel(1);
            (rx, tx)
        })
        .unzip();
    (
        Arc::new(Gate {
            entered: tx,
            releases: Mutex::new(release),
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
            sim: SimRuntime::new(),
            forwarded: Mutex::new(None),
            catalog: std::sync::OnceLock::new(),
        }),
        rx,
        senders,
    )
}

#[test]
fn lifecycle_forwarded_completion_owns_identity_until_reply_publication_and_releases_on_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let (gate, _, _) = fixture(0);
    let node = Arc::new(Recorded::open(Backend(Arc::clone(&gate)), dir.path()).unwrap());
    node.create(spec()).unwrap();
    let (tx, rx) = mpsc::sync_channel(1);
    let replying = Arc::clone(&node);
    let call = Call::Signal {
        id: spec().id,
        signal: Signal::TERM,
    };
    assert!(matches!(
        Host::forward(
            node.as_ref(),
            call,
            Stdio::null(),
            Box::new(move |answer| {
                assert!(matches!(answer, Ok(Forwarded::Answer(_))));
                tx.send(replying.delete(&spec().id).is_err()).unwrap();
            }),
            Box::new(|_| {})
        ),
        Forward::Taken
    ));
    let early_delete = node.delete(&spec().id);
    let mut sibling = spec();
    sibling.id = BoxId::new("sibling").unwrap();
    node.create(sibling.clone()).unwrap();
    let pending = gate.forwarded.lock().unwrap().take().unwrap();
    pending(Ok(Forwarded::Answer(Box::new(Answer::Done))));
    let publication_held = rx.recv().unwrap();
    if early_delete.is_err() {
        node.delete(&spec().id).unwrap();
    }
    node.create(spec()).unwrap();
    // Default forward returns Back and drops its Reply: the worker must be able to take ownership.
    let fallback = Host::forward(
        node.as_ref(),
        Call::Start(spec().id),
        Stdio::null(),
        Box::new(|_| {}),
        Box::new(|_| {}),
    );
    assert!(matches!(fallback, Forward::Back(_)));
    node.delete(&spec().id).unwrap();
    node.delete(&sibling.id).unwrap();
    assert!(
        early_delete.is_err(),
        "a callback still owned this identity when delete released it"
    );
    assert!(
        publication_held,
        "reply publication reentered deletion before ownership was released"
    );
    assert!(gate.sim.list().unwrap().is_empty());
}

#[test]
fn lifecycle_failed_create_keeps_the_exact_record_until_physical_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let (gate, _, _) = fixture(0);
    let node = Recorded::open(Backend(Arc::clone(&gate)), dir.path()).unwrap();
    gate.fail.store(true, Ordering::SeqCst);
    let expected = spec();
    assert!(node.create(expected.clone()).is_err());
    assert_eq!(gate.sim.list().unwrap().len(), 1);
    let retained = node.books.store.get(&expected.id).unwrap();
    let second = node.create(expected.clone());
    let unchanged = node.books.store.get(&expected.id).unwrap();
    node.delete(&expected.id).unwrap();
    assert!(matches!(second, Err(Refusal::Exists(_))));
    assert_eq!(retained, unchanged);
    assert!(
        retained.is_some(),
        "failed create discarded its only durable cleanup identity"
    );
    assert_eq!(retained.unwrap().unwrap().phase, Phase::Creating);
    assert!(gate.sim.list().unwrap().is_empty());
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn lifecycle_same_spec_aba_cannot_publish_a_new_attempt_from_an_old_completion() {
    let dir = tempfile::tempdir().unwrap();
    let (gate, entered, release) = fixture(2);
    let node = Arc::new(Recorded::open(Backend(Arc::clone(&gate)), dir.path()).unwrap());
    let start = |node: Arc<Recorded<Backend>>| std::thread::spawn(move || node.create(spec()));
    let a = start(Arc::clone(&node));
    entered.recv().unwrap();
    let deletion = node.delete(&spec().id);
    let overlaps = deletion.is_ok();
    let mut crossed_phase = None;
    if overlaps {
        // Exercise the old tree's actual A-delete-B-A-completion interleaving before asserting.
        let b = start(Arc::clone(&node));
        entered.recv().unwrap();
        release[0].send(()).unwrap();
        a.join().unwrap().unwrap();
        crossed_phase = node
            .books
            .store
            .get(&spec().id)
            .unwrap()
            .map(|r| r.unwrap().phase);
        release[1].send(()).unwrap();
        b.join().unwrap().unwrap();
    } else {
        assert!(node.create(spec()).is_err());
        release[0].send(()).unwrap();
        a.join().unwrap().unwrap();
        node.delete(&spec().id).unwrap();
        release[1].send(()).unwrap();
        node.create(spec()).unwrap();
        entered.recv().unwrap();
    }
    node.delete(&spec().id).unwrap();
    assert!(gate.sim.list().unwrap().is_empty());
    assert!(
        !overlaps,
        "delete released A's identity while A still owned publication; late A wrote B phase {crossed_phase:?}"
    );
}

#[test]
fn lifecycle_recovery_reaches_cleanup_for_incomplete_create_without_ready_probe() {
    let dir = tempfile::tempdir().unwrap();
    let (gate, _, _) = fixture(0);
    let node = Recorded::open(Backend(Arc::clone(&gate)), dir.path()).unwrap();
    let expected = spec();
    gate.sim.create(expected.clone()).unwrap();
    node.books
        .store
        .put(&BoxRecord::new(
            expected.clone(),
            Phase::Creating,
            Vec::new(),
        ))
        .unwrap();
    let recovery = node.recover();
    let physically_gone = gate.sim.list().unwrap().is_empty();
    let record_gone = node.books.store.get(&expected.id).unwrap().is_none();
    if !physically_gone {
        gate.sim.delete(&expected.id).unwrap();
    }
    assert!(recovery.is_ok(), "{recovery:?}");
    assert!(physically_gone && record_gone);
}

mod preflight;
