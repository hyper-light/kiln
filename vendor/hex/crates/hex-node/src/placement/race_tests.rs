//! A Hex admission and another word on the view, racing on two threads (joint plan step 13, CV3; AC-T7.4.1). Since
//! e9a61783 the view's calls and rebook take the Driver's core lock alone, while an admission (`Driver::hear`'s Start)
//! lets that lock go around its backend's check, between its gate and its commit. Each test holds an admission at
//! that seam, a runtime whose `classes` waits at a gate, while a holder reserves, a holder's sync adopts what its work
//! still holds, or the learner rebooks a box past the room; then lets it on. Neither side may pass the room check on
//! room the other took, and nothing new is admitted while the view is over.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use hex_api::capacity::{Event, Holder, Out};
use hex_api::sim::SimRuntime;
use hex_api::{BoxId, BoxInfo, BoxSpec, Proc, Program, Refusal, Runtime, Signal, Stdio, Usage};
use hex_core::patience::Patience;
use hex_sim::index::Pick;
use hex_sim::model::{Class, NodeId, NodeSpec};
use hex_sim::peer::{Budget, Msg};

use super::tests::{cpu, demand_of, four_cpus, holding, quiesce, started, talk};
use crate::placement::{Driver, Envelope, Notice, Runtimes, Step};

// No recorded operation makes these facts: each is waited for measuring, for the fact alone (hex_testkit::waits).
static PARKED: Patience = Patience::measuring("test.hex_node.capacity_race.parked");
static LET_ON: Patience = Patience::measuring("test.hex_node.capacity_race.let_on");
static ENDED: Patience = Patience::measuring("test.hex_node.capacity_race.ended");

/// The seam: once armed, the next `classes` call says it is there and waits for the test's word.
#[derive(Default)]
struct Gate {
    armed: AtomicBool,
    parked: AtomicBool,
    open: AtomicBool,
}

/// The node's runtime, its `classes` held at the gate once armed: the backend check an admission makes with the
/// core lock let go.
struct Gated {
    inner: SimRuntime,
    gate: Arc<Gate>,
}

impl Runtime for Gated {
    fn classes(&self) -> Vec<hex_api::Class> {
        if self.gate.armed.swap(false, Ordering::SeqCst) {
            self.gate.parked.store(true, Ordering::SeqCst);
            hex_testkit::waits::until(&LET_ON, "the test's word to go on", || {
                self.gate.open.load(Ordering::SeqCst)
            })
            .unwrap();
        }
        self.inner.classes()
    }
    fn capacity(&self, class: hex_api::Class) -> Result<Option<hex_api::Capacity>, Refusal> {
        self.inner.capacity(class)
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.inner.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.inner.start(id)
    }
    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.inner.exec(id, program, stdio)
    }
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.inner.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        self.inner.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.inner.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.inner.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.inner.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.inner.delete(id)
    }
}

/// A node of 4 CPUs on the gated runtime, its view open for one holder, and that holder joined.
fn booted(dir: &std::path::Path, gate: &Arc<Gate>) -> (Arc<Driver>, Holder) {
    let runtime: Arc<dyn Runtime> = Arc::new(Gated {
        inner: SimRuntime::default(),
        gate: Arc::clone(gate),
    });
    let driver = Driver::boot(
        NodeId(0),
        &[NodeSpec::new(NodeId(0), four_cpus())],
        &[],
        Budget { owned: 4, ran: 4 },
        Pick::default(),
        dir,
        Runtimes::new([(Class::Oci, runtime)]).unwrap(),
    )
    .unwrap();
    driver
        .refresh_node(NodeSpec::new(NodeId(0), four_cpus()))
        .unwrap();
    driver.open_view(&dir.join("capacity"), 1).unwrap();
    let mut kiln = Holder::start("kiln-runner".into(), 1, 4, &[]).unwrap();
    let hello = kiln.connect().unwrap();
    let joined = talk(&driver, &mut kiln, vec![hello]);
    assert!(
        matches!(joined.as_slice(), [Event::Synced { agree: true, .. }]),
        "{joined:?}"
    );
    (Arc::new(driver), kiln)
}

/// The start a submit sent this node.
fn start_of(step: Step) -> Envelope {
    let mut send = step.send;
    assert_eq!(send.len(), 1, "{send:?}");
    let (to, envelope) = send.remove(0);
    assert!(
        to == NodeId(0) && matches!(envelope.msg, Msg::Start { .. }),
        "{envelope:?}"
    );
    envelope
}

/// Holds `start`'s admission at its backend's check on a thread of its own, says `word` meanwhile, then lets it on:
/// what its step and every message after it said.
fn raced(driver: &Arc<Driver>, gate: &Gate, start: Envelope, word: impl FnOnce()) -> Vec<Notice> {
    gate.armed.store(true, Ordering::SeqCst);
    let admission = {
        let driver = Arc::clone(driver);
        std::thread::spawn(move || driver.hear(NodeId(0), start))
    };
    hex_testkit::waits::until(&PARKED, "the admission at its backend's check", || {
        gate.parked.load(Ordering::SeqCst)
    })
    .unwrap();
    word();
    gate.open.store(true, Ordering::SeqCst);
    hex_testkit::waits::until(&ENDED, "the admission's end", || admission.is_finished()).unwrap();
    let step = admission.join().unwrap().unwrap();
    quiesce(driver, step)
}

/// AC-T7.4.1, CV3: the node admits a box of 3 of its 4 CPUs, held between its room check and its commit while a
/// holder reserves 2; expect the holder granted, the node's box refused, and 2 CPUs held: both never pass on the
/// same room.
#[test]
fn a_holders_reserve_inside_an_admission_leaves_it_refused() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Gate::default());
    let (driver, mut kiln) = booted(dir.path(), &gate);
    let (d, p) = demand_of(3000);
    let (_, step) = driver.submit(d, p).unwrap();
    let notices = raced(&driver, &gate, start_of(step), || {
        let (run, msg) = kiln.reserve(cpu(2000)).unwrap();
        assert_eq!(
            talk(&driver, &mut kiln, vec![msg]),
            vec![Event::Granted(run)]
        );
    });
    assert!(
        !started(&notices),
        "the holder's 2 CPUs and the node's 3 were both admitted: {notices:?}"
    );
    assert_eq!(driver.view().unwrap().held.cpu_milli, 2000);
}

/// AC-T7.4.1, CV5 and CV6: a holder holding 2.5 of 4 CPUs drops and the person releases it; the node places a box of
/// 2, then one of 1.5, held between its room check and its commit while the holder, whose work ran on, rejoins and
/// syncs; expect the sync to adopt the 2.5 (the view over), the node's 1.5 refused, and 4.5 CPUs held: nothing new
/// admitted while the view is over.
#[test]
fn a_holders_sync_adopting_past_the_room_inside_an_admission_leaves_it_refused() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Gate::default());
    let (driver, mut kiln) = booted(dir.path(), &gate);
    let (run, msg) = kiln.reserve(cpu(2500)).unwrap();
    assert_eq!(
        talk(&driver, &mut kiln, vec![msg]),
        vec![Event::Granted(run)]
    );
    driver.view_link_down(1).unwrap();
    kiln.link_down(&mut Out::default());
    assert!(driver.view_forget("kiln-runner").unwrap().is_ok());
    let (d, p) = demand_of(2000);
    let (_, step) = driver.submit(d, p).unwrap();
    assert!(started(&quiesce(&driver, step)));
    let (d, p) = demand_of(1500);
    let (_, step) = driver.submit(d, p).unwrap();
    let notices = raced(&driver, &gate, start_of(step), || {
        let hello = kiln.connect().unwrap();
        let joined = talk(&driver, &mut kiln, vec![hello]);
        assert!(
            matches!(
                joined.as_slice(),
                [Event::Synced {
                    agree: true,
                    adopted: 1,
                    ..
                }]
            ),
            "{joined:?}"
        );
    });
    assert!(
        !started(&notices),
        "the node admitted 1.5 CPUs into a view the holder's sync put over: {notices:?}"
    );
    let view = driver.view().unwrap();
    assert_eq!((view.room.cpu_milli, view.held.cpu_milli), (4000, 4500));
}

/// AC-T7.4.1, joint plan steps 13 and 14: the node runs a box booked at 1 of 4 CPUs and places one of 1.5, held
/// between its room check and its commit while the learner rebooks the first at 5, past the room; expect the node's
/// 1.5 refused and 5 CPUs held.
#[test]
fn a_rebook_past_the_room_inside_an_admission_leaves_it_refused() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Gate::default());
    let (driver, _kiln) = booted(dir.path(), &gate);
    let (d, p) = demand_of(1000);
    let (vm, step) = driver.submit(d, p).unwrap();
    assert!(started(&quiesce(&driver, step)));
    let (d, p) = demand_of(1500);
    let (_, step) = driver.submit(d, p).unwrap();
    let notices = raced(&driver, &gate, start_of(step), || {
        assert_eq!(driver.rebook(&[(vm, holding(5000))]).unwrap(), 1);
    });
    assert!(
        !started(&notices),
        "the node admitted 1.5 CPUs into a view its rebook put over: {notices:?}"
    );
    assert_eq!(driver.view().unwrap().held.cpu_milli, 5000);
}
