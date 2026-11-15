//! The node's capacity view (joint plan step 13; AC-T7.4.1): its shell over the node's record files held
//! to the conformance suite, and the node's own admission and a holder's reserve checking one view.

use std::collections::VecDeque;
use std::sync::Arc;

use hex_api::capacity::{
    Amounts, CapacityView, Event, Holder, Host, Out, Scope, ToHolder, ToKeeper, Why, conformance,
};
use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Refusal, Runtime};
use hex_sim::index::Pick;
use hex_sim::model::{Ask, Class, Classes, Demand, Kind, NodeId, NodeSpec, Resources};
use hex_sim::peer::{self, Budget, Stop};

use super::View;
use crate::placement::{Driver, Notice, Payload, Runtimes, Step};

const HOLDERS: usize = 2;

fn refusal(stop: Stop) -> Refusal {
    Refusal::Broken(format!("{stop:?}"))
}

pub(super) fn cpu(milli: u64) -> Amounts {
    Amounts {
        cpu_milli: milli,
        ..Amounts::ZERO
    }
}

/// The node's view as the conformance suite drives it: its records a folder of their own on disk, the
/// node's own held set by the suite.
struct NodeHost {
    dir: tempfile::TempDir,
    view: View,
    local: Amounts,
    room: Amounts,
}

impl NodeHost {
    fn new() -> Result<Self, Refusal> {
        let dir = tempfile::tempdir().map_err(|e| Refusal::Broken(e.to_string()))?;
        let mut view = View::open(dir.path(), HOLDERS).map_err(refusal)?;
        view.set_room(conformance::ROOM);
        Ok(Self {
            dir,
            view,
            local: Amounts::ZERO,
            room: conformance::ROOM,
        })
    }
}

impl CapacityView for NodeHost {
    fn hear(&mut self, link: u64, msg: ToKeeper) -> Result<ToHolder, Refusal> {
        self.view
            .hear(link, msg, self.local)
            .map(|(answer, _)| answer)
            .map_err(refusal)
    }

    fn link_down(&mut self, link: u64) -> Result<(), Refusal> {
        self.view.link_down(link);
        Ok(())
    }

    fn hints(&mut self) -> Result<Vec<(u64, ToHolder)>, Refusal> {
        self.view.hints(self.local).map_err(refusal)
    }
}

impl Host for NodeHost {
    fn set_room(&mut self, room: Amounts) -> Result<(), Refusal> {
        self.room = room;
        self.view.set_room(room);
        Ok(())
    }

    fn set_local(&mut self, local: Amounts) -> Result<(), Refusal> {
        self.local = local;
        Ok(())
    }

    fn restart(&mut self) -> Result<(), Refusal> {
        self.view = View::open(self.dir.path(), HOLDERS).map_err(refusal)?;
        self.view.set_room(self.room);
        Ok(())
    }
}

/// AC-T7.4.1, CLAUDE.md §9: run the capacity view's conformance suite against the node's view over its
/// record files, restarted from them; expect every case to pass.
#[test]
fn the_nodes_view_passes_the_conformance_suite() {
    let mut make = || -> Result<Box<dyn Host>, Refusal> { Ok(Box::new(NodeHost::new()?)) };
    let failures = conformance::view(&mut make);
    assert!(failures.is_empty(), "{failures:#?}");
}

pub(super) fn demand_of(cpu_milli: u64) -> (Demand, Payload) {
    let need = Resources {
        cpu_milli,
        mem_bytes: hex_api::MIN_MEMORY,
        ..Resources::default()
    };
    let demand = Demand {
        need,
        kind: Kind::Plain,
        class: Class::Oci,
        ask: Ask::default(),
    };
    let payload = Payload {
        spec: BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: BoxId::new("view-box").unwrap(),
            bounds: Bounds {
                cpu_max_milli: Some(u32::try_from(cpu_milli).unwrap()),
                memory_max: Some(need.mem_bytes),
                ..Bounds::default()
            },
            class: ClassSpec::Process,
            owner: None,
            band: 0,
        },
        start: false,
    };
    (demand, payload)
}

/// Delivers a step's messages to this one node until it has nothing more to say: its notices.
pub(super) fn quiesce(driver: &Driver, step: Step) -> Vec<Notice> {
    let mut send = VecDeque::from(step.send);
    let mut notices = step.notices;
    while let Some((to, envelope)) = send.pop_front() {
        assert_eq!(to, NodeId(0));
        let next = driver.hear(NodeId(0), envelope).unwrap();
        send.extend(next.send);
        notices.extend(next.notices);
        assert!(notices.len() < 64, "the node did not quiesce");
    }
    notices
}

pub(super) fn started(notices: &[Notice]) -> bool {
    notices.iter().any(|n| matches!(n, Notice::Started { .. }))
}

/// Says `msgs` to the node's keeper on link 1, and its answers to `holder`, until neither has more to say.
pub(super) fn talk(driver: &Driver, holder: &mut Holder, msgs: Vec<ToKeeper>) -> Vec<Event> {
    let mut queue = VecDeque::from(msgs);
    let mut events = Vec::new();
    while let Some(msg) = queue.pop_front() {
        let answer = driver.view_hear(1, msg).unwrap();
        let mut out = Out::default();
        holder.hear(answer, &mut out);
        queue.extend(out.send);
        events.extend(out.events);
    }
    events
}

/// AC-T7.4.1, CV3: a holder reserves on a node before and after its facts give the view its room, the
/// node places a box of its own beside the reservation, then after it ends, and the holder asks again
/// beside the node's box; expect nothing admitted before the room is known, Hex's box refused while the
/// holder's reservation leaves too little and started once it is released, the holder then refused for
/// the room Hex's box holds with the view saying what is free, and the holder owed a hint of Hex's box.
#[test]
fn the_nodes_admission_and_a_holders_reserve_check_one_view() {
    let dir = tempfile::tempdir().unwrap();
    let room = Resources {
        cpu_milli: 2000,
        mem_bytes: 8 * hex_api::MIN_MEMORY,
        ..Resources::default()
    };
    let runtime: Arc<dyn Runtime> = Arc::new(hex_api::sim::SimRuntime::default());
    let driver = Driver::boot(
        NodeId(0),
        &[NodeSpec::new(NodeId(0), room)],
        &[],
        Budget { owned: 4, ran: 4 },
        Pick::default(),
        dir.path(),
        Runtimes::new([(Class::Oci, runtime)]).unwrap(),
    )
    .unwrap();
    driver.open_view(&dir.path().join("capacity"), 1).unwrap();
    let mut kiln = Holder::start("kiln-runner".into(), 1, 4, &[]).unwrap();
    let hello = kiln.connect().unwrap();
    let joined = talk(&driver, &mut kiln, vec![hello]);
    assert!(
        matches!(joined.as_slice(), [Event::Synced { agree: true, .. }]),
        "{joined:?}"
    );
    let (early, msg) = kiln.reserve(cpu(1)).unwrap();
    assert_eq!(
        talk(&driver, &mut kiln, vec![msg]),
        vec![Event::Refused(early, Why::Room)]
    );

    driver.refresh_node(NodeSpec::new(NodeId(0), room)).unwrap();
    let (run, msg) = kiln.reserve(cpu(1500)).unwrap();
    assert_eq!(
        talk(&driver, &mut kiln, vec![msg]),
        vec![Event::Granted(run)]
    );
    let (d, p) = demand_of(1000);
    let (_, step) = driver.submit(d, p).unwrap();
    assert!(!started(&quiesce(&driver, step)));

    let mut out = Out::default();
    kiln.release(run, &mut out);
    assert_eq!(
        talk(&driver, &mut kiln, out.send),
        vec![Event::Released(run)]
    );
    assert!(started(&quiesce(&driver, driver.retry().unwrap())));

    let hints = driver.view_hints().unwrap();
    assert_eq!(hints.len(), 1, "{hints:?}");
    let mut out = Out::default();
    for (_, hint) in hints {
        kiln.hear(hint, &mut out);
    }
    let (late, msg) = kiln.reserve(cpu(1500)).unwrap();
    assert_eq!(
        talk(&driver, &mut kiln, vec![msg]),
        vec![Event::Refused(late, Why::Room)]
    );
    assert_eq!(kiln.view().unwrap().free().cpu_milli, 1000);
    assert_eq!(driver.view().unwrap().held.cpu_milli, 1000);
}

/// A node's runtime that keeps each word of the machine's capacity view's keeper it is told (the review's A-10), its boxes
/// the simulator's.
#[derive(Default)]
struct Keeping {
    sim: hex_api::sim::SimRuntime,
    words: std::sync::Mutex<Vec<hex_api::Reserved>>,
}

impl Runtime for Keeping {
    fn classes(&self) -> Vec<hex_api::Class> {
        self.sim.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<hex_api::BoxInfo, Refusal> {
        self.sim.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.start(id)
    }
    fn exec(
        &self,
        id: &BoxId,
        program: hex_api::Program,
        stdio: hex_api::Stdio,
    ) -> Result<Box<dyn hex_api::Proc>, Refusal> {
        self.sim.exec(id, program, stdio)
    }
    fn run(
        &self,
        spec: BoxSpec,
        program: hex_api::Program,
        stdio: hex_api::Stdio,
    ) -> Result<Box<dyn hex_api::Proc>, Refusal> {
        self.sim.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: hex_api::Signal) -> Result<(), Refusal> {
        self.sim.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<hex_api::BoxInfo, Refusal> {
        self.sim.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<hex_api::Usage, Refusal> {
        self.sim.usage(id)
    }
    fn list(&self) -> Result<Vec<hex_api::BoxInfo>, Refusal> {
        self.sim.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.delete(id)
    }
    fn reserved(&self, word: hex_api::Reserved) -> Result<(), Refusal> {
        self.words.lock().unwrap().push(word);
        Ok(())
    }
}

/// The review's A-10 (AC-T7.4.1; ROOT, 2026-11-14): what the machine's capacity view's holders reserve reaches the node's
/// charge learner, its keeper's word woken at each change of it, off the holders' path. Do boot a node whose runtime keeps
/// each word it is told, its view open, kiln's holder joined and its word's wake set; have the holder reserve 1.5 CPUs,
/// granted; then tell the word. Expect the grant to wake it, and the runtime told the keeper's epoch and the 1.5 CPUs
/// reserved, once, and nothing refused.
#[test]
fn a10_a_holders_reserve_wakes_and_tells_the_node_its_keepers_word() {
    let dir = tempfile::tempdir().unwrap();
    let room = Resources {
        cpu_milli: 2000,
        mem_bytes: 8 * hex_api::MIN_MEMORY,
        ..Resources::default()
    };
    let keeping = Arc::new(Keeping::default());
    let runtime: Arc<dyn Runtime> = keeping.clone();
    let driver = Driver::boot(
        NodeId(0),
        &[NodeSpec::new(NodeId(0), room)],
        &[],
        Budget { owned: 4, ran: 4 },
        Pick::default(),
        dir.path(),
        Runtimes::new([(Class::Oci, runtime)]).unwrap(),
    )
    .unwrap();
    driver.refresh_node(NodeSpec::new(NodeId(0), room)).unwrap();
    driver.open_view(&dir.path().join("capacity"), 1).unwrap();
    let (wake, woken) = hex_wait::mpsc::sync_channel(1).unwrap();
    driver.on_reserved(wake).unwrap();
    let mut kiln = Holder::start("kiln-runner".into(), 1, 4, &[]).unwrap();
    let hello = kiln.connect().unwrap();
    talk(&driver, &mut kiln, vec![hello]);
    let _ = woken.try_recv();
    let (run, msg) = kiln.reserve(cpu(1500)).unwrap();
    assert_eq!(
        talk(&driver, &mut kiln, vec![msg]),
        vec![Event::Granted(run)]
    );
    assert!(
        matches!(woken.try_recv(), Ok(())),
        "a grant wakes its word's thread"
    );
    assert_eq!(driver.tell_reserved().unwrap(), Vec::new());
    let keeper = driver.reserved_word().unwrap().unwrap().keeper;
    assert_eq!(
        *keeping.words.lock().unwrap(),
        [hex_api::Reserved {
            keeper,
            reserved: hex_api::CapacityResources {
                cpu_milli: 1500,
                mem_bytes: 0,
                disk_bytes: 0,
            },
        }]
    );
}

/// A node whose facts say `room`, its capacity view open for one holder, and that holder joined.
fn booted(dir: &std::path::Path, room: Resources) -> (Driver, Holder) {
    let runtime: Arc<dyn Runtime> = Arc::new(hex_api::sim::SimRuntime::default());
    let driver = Driver::boot(
        NodeId(0),
        &[NodeSpec::new(NodeId(0), room)],
        &[],
        Budget { owned: 4, ran: 4 },
        Pick::default(),
        dir,
        Runtimes::new([(Class::Oci, runtime)]).unwrap(),
    )
    .unwrap();
    driver.refresh_node(NodeSpec::new(NodeId(0), room)).unwrap();
    driver.open_view(&dir.join("capacity"), 1).unwrap();
    let mut kiln = Holder::start("kiln-runner".into(), 1, 4, &[]).unwrap();
    let hello = kiln.connect().unwrap();
    let joined = talk(&driver, &mut kiln, vec![hello]);
    assert!(
        matches!(joined.as_slice(), [Event::Synced { agree: true, .. }]),
        "{joined:?}"
    );
    (driver, kiln)
}

/// What the learner says a box holds now: `cpu_milli`, and the least memory a box holds.
pub(super) fn holding(cpu_milli: u64) -> hex_api::CapacityResources {
    hex_api::CapacityResources {
        cpu_milli,
        mem_bytes: hex_api::MIN_MEMORY,
        disk_bytes: 0,
    }
}

pub(super) fn four_cpus() -> Resources {
    Resources {
        cpu_milli: 4000,
        mem_bytes: 8 * hex_api::MIN_MEMORY,
        ..Resources::default()
    }
}

/// AC-T7.4.1, joint plan steps 13 and 14 (C8): the node runs a box booked at 3 of its 4 CPUs and a holder
/// asks 2; then the learner rebooks the box at 1 CPU, the holder asks again, and the node places a box of
/// 1 CPU, then another; expect the first ask refused for room and the second granted, the node's first new
/// box started in the room the holding left and its second refused, and the view's room the machine's 4
/// CPUs throughout.
#[test]
fn a_falling_learned_holding_frees_room_while_the_physical_room_stays() {
    let dir = tempfile::tempdir().unwrap();
    let (driver, mut kiln) = booted(dir.path(), four_cpus());
    let (d, p) = demand_of(3000);
    let (vm, step) = driver.submit(d, p).unwrap();
    assert!(started(&quiesce(&driver, step)));
    let (early, msg) = kiln.reserve(cpu(2000)).unwrap();
    assert_eq!(
        talk(&driver, &mut kiln, vec![msg]),
        vec![Event::Refused(early, Why::Room)]
    );
    assert_eq!(
        driver
            .rebook(&[(peer::Holder::Box(vm), holding(1000))])
            .unwrap(),
        1
    );
    let view = driver.view().unwrap();
    assert_eq!((view.room.cpu_milli, view.held.cpu_milli), (4000, 1000));
    let (late, msg) = kiln.reserve(cpu(2000)).unwrap();
    assert_eq!(
        talk(&driver, &mut kiln, vec![msg]),
        vec![Event::Granted(late)]
    );
    let (d, p) = demand_of(1000);
    let (_, step) = driver.submit(d, p).unwrap();
    assert!(started(&quiesce(&driver, step)));
    let (d, p) = demand_of(1000);
    let (_, step) = driver.submit(d, p).unwrap();
    assert!(!started(&quiesce(&driver, step)));
    let view = driver.view().unwrap();
    assert_eq!(
        (
            view.room.cpu_milli,
            view.held.cpu_milli,
            view.free().cpu_milli
        ),
        (4000, 4000, 0)
    );
}

/// AC-T7.4.1, joint plan steps 13 and 14: a box booked at 3 of 4 CPUs is rebooked at 1 and a holder takes
/// 2.5; the box then ends in the middle of the learner's window, before any rebook, and right after the
/// node places a box of 2 CPUs and the holder asks 2, then 1.5; expect the node's box and the holder's 2
/// refused, its 1.5 granted, and what is held never past the machine's 4 CPUs.
#[test]
fn a_box_ending_mid_window_lets_nothing_past_the_physical_room() {
    let dir = tempfile::tempdir().unwrap();
    let (driver, mut kiln) = booted(dir.path(), four_cpus());
    let (d, p) = demand_of(3000);
    let (vm, step) = driver.submit(d, p).unwrap();
    assert!(started(&quiesce(&driver, step)));
    driver
        .rebook(&[(peer::Holder::Box(vm), holding(1000))])
        .unwrap();
    let (first, msg) = kiln.reserve(cpu(2500)).unwrap();
    assert_eq!(
        talk(&driver, &mut kiln, vec![msg]),
        vec![Event::Granted(first)]
    );
    let ended = quiesce(&driver, driver.delete(vm).unwrap());
    assert!(ended.iter().any(|n| matches!(n, Notice::Ended { .. })));
    let (d, p) = demand_of(2000);
    let (_, step) = driver.submit(d, p).unwrap();
    assert!(
        !started(&quiesce(&driver, step)),
        "the node placed into room the ended box's holding no longer leaves"
    );
    let (big, msg) = kiln.reserve(cpu(2000)).unwrap();
    assert_eq!(
        talk(&driver, &mut kiln, vec![msg]),
        vec![Event::Refused(big, Why::Room)]
    );
    let (fits, msg) = kiln.reserve(cpu(1500)).unwrap();
    assert_eq!(
        talk(&driver, &mut kiln, vec![msg]),
        vec![Event::Granted(fits)]
    );
    let view = driver.view().unwrap();
    assert!(
        view.room.cpu_milli == 4000 && view.held.fits(view.room),
        "{view:?}"
    );
}

/// AC-T7.4.1, joint plan steps 13 and 14: a box booked at 1 of 4 CPUs grows and the learner rebooks it at
/// 5, past the machine's room; the node then places a box of 1 CPU and a holder asks 1; then the holding
/// falls back to 1 CPU and both try again; expect the view over by 1 CPU, the node's box refused typed as
/// local admission and the holder's ask refused for room, then both admitted.
#[test]
fn a_holding_past_the_room_blocks_the_nodes_admission_typed() {
    let dir = tempfile::tempdir().unwrap();
    let (driver, mut kiln) = booted(dir.path(), four_cpus());
    let (d, p) = demand_of(1000);
    let (vm, step) = driver.submit(d, p).unwrap();
    assert!(started(&quiesce(&driver, step)));
    driver
        .rebook(&[(peer::Holder::Box(vm), holding(5000))])
        .unwrap();
    assert_eq!(driver.view().unwrap().over().cpu_milli, 1000);
    let (d, p) = demand_of(1000);
    let (_, step) = driver.submit(d, p).unwrap();
    let notices = quiesce(&driver, step);
    assert!(
        notices.iter().any(|n| matches!(
            n,
            Notice::Refused {
                refusal: Refusal::Invalid {
                    what: "local admission",
                    ..
                },
                ..
            }
        )),
        "{notices:?}"
    );
    let (early, msg) = kiln.reserve(cpu(1000)).unwrap();
    assert_eq!(
        talk(&driver, &mut kiln, vec![msg]),
        vec![Event::Refused(early, Why::Room)]
    );
    driver
        .rebook(&[(peer::Holder::Box(vm), holding(1000))])
        .unwrap();
    assert!(started(&quiesce(&driver, driver.retry().unwrap())));
    let (late, msg) = kiln.reserve(cpu(1000)).unwrap();
    assert_eq!(
        talk(&driver, &mut kiln, vec![msg]),
        vec![Event::Granted(late)]
    );
}

/// AC-T7.4.1, CV9: a holder on an endpoint of its own reaches the node's own keeper, the Driver's view served on its
/// QUIC port, reserves 3 of 4 CPUs, and the node then places a box of 2 CPUs and one of 1; expect the grant, the
/// node's 2 CPUs refused and its 1 started, and the holder told over QUIC the view with all 4 CPUs held.
#[test]
fn a_holder_reaches_the_nodes_keeper_over_quic() {
    use hex_core::patience::Patience;
    use hex_quic::capacity::{CAPACITY_PORT, Link, Server};
    use hex_quic::enroll::{self, sim::SimCa};
    use hex_quic::{Config, Endpoint, Refused, Shape};
    use hex_testkit::waits::follow::{self, Party};

    // The keeper's next word over loopback is waited for following the keeper by its life alone (decision 9): past the
    // slowest answer seen the wait goes on, and ends with the word or the link's end, which the read says itself. The
    // dial and each write wait for their answers with no deadline of the test's: the driver ends a dial nothing answers
    // once QUIC's idle timeout passes (RFC 9000 §10.1), and a write cut at a slice's end would leave half a message.
    /// The keeper's next word to the holder: its answer to what it said, or a hint.
    static HEAR: Patience = Patience::measuring("test.hex_node.capacity.hear");
    // By the life of the keeper's serving thread (the review's C-5), and the link's end, which the read says itself.
    let heard = |link: &mut Link, keeper: &std::thread::JoinHandle<Refused>| {
        follow::within(
            &HEAR,
            "the keeper's next word",
            || {
                if keeper.is_finished() {
                    Party::Ended
                } else {
                    Party::Live
                }
            },
            |by| match link.hear(by) {
                Err(Refused::Deadline { .. }) => None,
                heard => Some(heard),
            },
        )
        .unwrap()
        .unwrap()
    };

    let dir = tempfile::tempdir().unwrap();
    let runtime: Arc<dyn Runtime> = Arc::new(hex_api::sim::SimRuntime::default());
    let room = four_cpus();
    let driver = Arc::new(
        Driver::boot(
            NodeId(0),
            &[NodeSpec::new(NodeId(0), room)],
            &[],
            Budget { owned: 4, ran: 4 },
            Pick::default(),
            dir.path(),
            Runtimes::new([(Class::Oci, runtime)]).unwrap(),
        )
        .unwrap(),
    );
    driver.refresh_node(NodeSpec::new(NodeId(0), room)).unwrap();
    driver.open_view(&dir.path().join("capacity"), 1).unwrap();

    let ca = SimCa::new().unwrap();
    let enrolled = |name: &str| {
        ca.admit(name, name.as_bytes());
        enroll::enroll(&ca, name, name.as_bytes().to_vec()).unwrap()
    };
    let loopback = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0));
    let node = Endpoint::start(Config {
        credentials: enrolled("node.capacity.test"),
        bind: loopback,
        listen: true,
        serves: vec![CAPACITY_PORT],
        shape: Shape {
            connections: 2,
            lanes: 8,
        },
    })
    .unwrap();
    let server = Server::new(
        super::super::DriverView(Arc::clone(&driver)),
        &["kiln.capacity.test".to_owned()],
        Arc::new(|_: &str| {}),
    );
    let keeper = {
        let server = Arc::clone(&server);
        let acceptor = node.acceptor(CAPACITY_PORT);
        std::thread::spawn(move || server.serve(acceptor))
    };
    let kiln_end = Endpoint::start(Config {
        credentials: enrolled("kiln.capacity.test"),
        bind: loopback,
        listen: false,
        serves: Vec::new(),
        shape: Shape {
            connections: 1,
            lanes: 8,
        },
    })
    .unwrap();
    let peer = kiln_end
        .dial("node.capacity.test", node.local_addr(), None)
        .unwrap();
    let mut link = Link::open(&kiln_end, peer).unwrap();
    let mut kiln = Holder::start("kiln.capacity.test".into(), 1, 4, &[]).unwrap();
    let mut said = VecDeque::from(vec![kiln.connect().unwrap()]);
    let mut events = Vec::new();
    let mut converse = |link: &mut Link, kiln: &mut Holder, said: &mut VecDeque<ToKeeper>| {
        while let Some(msg) = said.pop_front() {
            link.say(&msg, None).unwrap();
            let mut out = Out::default();
            kiln.hear(heard(link, &keeper), &mut out);
            said.extend(out.send);
            events.extend(out.events);
        }
    };
    converse(&mut link, &mut kiln, &mut said);
    let (run, msg) = kiln.reserve(cpu(3000)).unwrap();
    said.push_back(msg);
    converse(&mut link, &mut kiln, &mut said);
    assert!(
        matches!(events.as_slice(), [Event::Synced { agree: true, .. }, Event::Granted(id)] if *id == run),
        "{events:?}"
    );
    let (d, p) = demand_of(2000);
    let (_, step) = driver.submit(d, p).unwrap();
    assert!(!started(&quiesce(&driver, step)));
    let (d, p) = demand_of(1000);
    let (_, step) = driver.submit(d, p).unwrap();
    assert!(started(&quiesce(&driver, step)));
    server.hint().unwrap();
    let ToHolder::Hint { view, .. } = heard(&mut link, &keeper) else {
        panic!("no hint");
    };
    assert_eq!((view.room.cpu_milli, view.held.cpu_milli), (4000, 4000));
}

/// AC-T7.4.1, CV12 (withdrawn): on a machine of 8 GiB a holder whose hello names its work's cgroup reserves 4 GiB, and its
/// work uses 3, so the node's facts measure 5; the node places a box of 4 GiB, then one of 1. Expect the box of 4 refused
/// and the box of 1 started: the holder's 3 GiB counted twice, missing from the measured room and held besides, which
/// admits less and never more than the machine holds, and nothing the hello named credited.
#[test]
fn cv12_withdrawn_a_holders_used_memory_is_counted_twice_never_credited() {
    const GIB: u64 = 1 << 30;
    let machine = |mem_bytes| Resources {
        cpu_milli: 4000,
        mem_bytes,
        ..Resources::default()
    };
    let demand_mem = |bytes| {
        let (mut demand, mut payload) = demand_of(1000);
        demand.need.mem_bytes = bytes;
        payload.spec.bounds.memory_max = Some(bytes);
        (demand, payload)
    };
    let dir = tempfile::tempdir().unwrap();
    let runtime: Arc<dyn Runtime> = Arc::new(hex_api::sim::SimRuntime::default());
    let driver = Driver::boot(
        NodeId(0),
        &[NodeSpec::new(NodeId(0), machine(8 * GIB))],
        &[],
        Budget { owned: 4, ran: 4 },
        Pick::default(),
        dir.path(),
        Runtimes::new([(Class::Oci, runtime)]).unwrap(),
    )
    .unwrap();
    driver
        .refresh_node(NodeSpec::new(NodeId(0), machine(8 * GIB)))
        .unwrap();
    driver.open_view(&dir.path().join("capacity"), 1).unwrap();
    let hello = ToKeeper::Hello {
        holder: "kiln-runner".into(),
        epoch: 1,
        scope: Some(Scope::Cgroup("kiln.slice/jobs".into())),
    };
    let mut kiln = Holder::start("kiln-runner".into(), 1, 4, &[]).unwrap();
    kiln.connect().unwrap();
    let joined = talk(&driver, &mut kiln, vec![hello]);
    assert!(
        matches!(joined.as_slice(), [Event::Synced { agree: true, .. }]),
        "{joined:?}"
    );
    let (run, msg) = kiln
        .reserve(Amounts {
            mem_bytes: 4 * GIB,
            ..Amounts::ZERO
        })
        .unwrap();
    assert_eq!(
        talk(&driver, &mut kiln, vec![msg]),
        vec![Event::Granted(run)]
    );
    driver
        .refresh_node(NodeSpec::new(NodeId(0), machine(5 * GIB)))
        .unwrap();
    let (d, p) = demand_mem(4 * GIB);
    let (_, step) = driver.submit(d, p).unwrap();
    assert!(
        !started(&quiesce(&driver, step)),
        "the holder's use was credited"
    );
    let view = driver.view().unwrap();
    assert_eq!((view.room.mem_bytes, view.free().mem_bytes), (5 * GIB, GIB));
    let (d, p) = demand_mem(GIB);
    let (_, step) = driver.submit(d, p).unwrap();
    assert!(started(&quiesce(&driver, step)));
}

/// AC-T7.4.1, CV8 (the open item "a scheduler with no runtime class keeps no holder reservation"): a node of four CPUs
/// with no runtime class of its own (a macOS machine without Hypervisor.framework, where Hex places nothing: its facts'
/// classes are none, so its own boxes' record bound is none) opens its view for one holder, and the holder reserves one
/// CPU. Expect it granted: a holder's reservations are bounded by what one sync lists, not by the boxes Hex would run
/// on the machine.
#[test]
fn cv8_a_node_with_no_runtime_class_of_its_own_keeps_a_holders_reservations() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = NodeSpec::new(NodeId(0), four_cpus());
    spec.classes = Classes::NONE;
    let driver = Driver::boot(
        NodeId(0),
        &[spec.try_clone().unwrap()],
        &[],
        Budget { owned: 1, ran: 0 },
        Pick::default(),
        dir.path(),
        Runtimes::new(std::iter::empty::<(Class, Arc<dyn Runtime>)>()).unwrap(),
    )
    .unwrap();
    driver.refresh_node(spec).unwrap();
    driver.open_view(&dir.path().join("capacity"), 1).unwrap();
    let mut kiln = Holder::start("kiln-runner".into(), 1, 4, &[]).unwrap();
    let hello = kiln.connect().unwrap();
    let joined = talk(&driver, &mut kiln, vec![hello]);
    assert!(
        matches!(joined.as_slice(), [Event::Synced { agree: true, .. }]),
        "{joined:?}"
    );
    let (run, msg) = kiln.reserve(cpu(1000)).unwrap();
    assert_eq!(
        talk(&driver, &mut kiln, vec![msg]),
        vec![Event::Granted(run)]
    );
}

/// AC-T7.4.1, CV8 (R03: the bound pinned at its edge): a holder's name is held to a record's bytes less its epoch field,
/// a u64 in decimal and one space. Do: open the view and say a hello naming a holder by the most bytes its bound takes, at
/// the widest epoch; then one naming another holder a byte longer. Expect: the first welcomed, its record exactly a
/// record's bytes; the second closed, its name invalid.
#[test]
fn a_holders_name_at_its_bound_fills_its_record_at_the_widest_epoch() {
    use hex_api::capacity::{Close, Invalid};
    let dir = tempfile::tempdir().unwrap();
    let mut view = View::open(dir.path(), HOLDERS).unwrap();
    view.set_room(conformance::ROOM);
    let most = crate::records::MAX_BYTES - super::EPOCH_FIELD;
    let hello = |name: String| ToKeeper::Hello {
        holder: name,
        epoch: u64::MAX,
        scope: None,
    };
    let (welcome, _) = view
        .hear(0, hello("h".repeat(most)), Amounts::ZERO)
        .unwrap();
    assert!(matches!(welcome, ToHolder::Welcome { .. }), "{welcome:?}");
    let record = std::fs::metadata(dir.path().join("holder-0.rec"))
        .unwrap()
        .len();
    assert_eq!(record, u64::try_from(crate::records::MAX_BYTES).unwrap());
    let (closed, _) = view
        .hear(1, hello("i".repeat(most + 1)), Amounts::ZERO)
        .unwrap();
    assert_eq!(
        closed,
        ToHolder::Closed {
            rejected: None,
            why: Close::Invalid(Invalid::Name),
        }
    );
}

/// An oci box's demand and payload: `cpu_milli` and the least memory a box holds, as [`demand_of`]'s process box, its
/// image Linux on x86_64, as a routed oci box names its image's platform.
fn oci_of(cpu_milli: u64) -> (Demand, Payload) {
    let (mut demand, mut payload) = demand_of(cpu_milli);
    demand.ask.os = Some(hex_sim::model::Os::Linux);
    demand.ask.arch = Some(hex_sim::model::Arch::X86_64);
    payload.spec.class = ClassSpec::Oci(Box::new(hex_api::OciSpec {
        rootfs: hex_api::Rootfs::Dir(std::path::PathBuf::from("/images/agent")),
        init: hex_api::Program::of(&["/bin/true"]),
        mounts: Vec::new(),
        hostname: None,
        network: hex_api::Network::Private,
        hosts: Vec::new(),
    }));
    (demand, payload)
}

/// A node's routes when its oci class runs in its node VM (joint plan step 39, V2): a process box's native one and an oci
/// box's NodeVm one.
fn node_vm_routes() -> Vec<hex_api::RouteOffer> {
    let route = |class, execution| hex_api::RouteOffer {
        key: hex_api::RouteKey {
            class,
            image: hex_api::ImagePlatform {
                os: hex_api::ImageOs::Linux,
                arch: hex_api::ImageArch::Amd64,
            },
            execution,
            gpu: hex_api::GpuEligibility::None,
        },
        cost: None,
        backing: None,
    };
    vec![
        route(hex_api::Class::Process, hex_api::Execution::Native),
        route(hex_api::Class::Oci, hex_api::Execution::NodeVm),
    ]
}

/// A runtime offering [`node_vm_routes`] as a node VM's node answers them, its boxes made as their routes ask.
struct Routed(hex_api::sim::SimRuntime);

impl Runtime for Routed {
    fn classes(&self) -> Vec<hex_api::Class> {
        vec![hex_api::Class::Process, hex_api::Class::Oci]
    }
    fn capacity(&self, _: hex_api::Class) -> Result<Option<hex_api::Capacity>, Refusal> {
        Ok(Some(hex_api::Capacity {
            own: Default::default(),
            id: "node-vm-host".into(),
            total: hex_api::CapacityResources::default(),
            free: hex_api::CapacityResources::default(),
            devices: Vec::new(),
            cost: None,
            routes: node_vm_routes(),
            device_routes: Vec::new(),
            backings: Vec::new(),
            unavailable: Vec::new(),
            learned: None,
            node_vms: Vec::new(),
            unlearned: None,
        }))
    }
    fn create(&self, spec: BoxSpec) -> Result<hex_api::BoxInfo, Refusal> {
        self.0.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.0.start(id)
    }
    fn exec(
        &self,
        id: &BoxId,
        program: hex_api::Program,
        stdio: hex_api::Stdio,
    ) -> Result<Box<dyn hex_api::Proc>, Refusal> {
        self.0.exec(id, program, stdio)
    }
    fn run(
        &self,
        spec: BoxSpec,
        program: hex_api::Program,
        stdio: hex_api::Stdio,
    ) -> Result<Box<dyn hex_api::Proc>, Refusal> {
        self.0.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: hex_api::Signal) -> Result<(), Refusal> {
        self.0.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<hex_api::BoxInfo, Refusal> {
        self.0.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<hex_api::Usage, Refusal> {
        self.0.usage(id)
    }
    fn list(&self) -> Result<Vec<hex_api::BoxInfo>, Refusal> {
        self.0.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.0.delete(id)
    }
}

/// A node of `room` whose oci class runs in its node VM (joint plan step 39, V2): its routes [`node_vm_routes`], its
/// Ready node VM's guest admitted at 1.5 CPUs and 3 of the least boxes' memory.
fn node_vm_node(room: Resources) -> NodeSpec {
    let mut spec = NodeSpec::new(NodeId(0), room);
    spec.routes = Some(
        hex_sim::model::RuntimeRoutes {
            admit: None,
            offers: node_vm_routes(),
            device_routes: Vec::new(),
            backings: Vec::new(),
            node_vm_room: Some(hex_api::CapacityResources {
                cpu_milli: 1_500,
                mem_bytes: 3 * hex_api::MIN_MEMORY,
                disk_bytes: 0,
            }),
        }
        .share()
        .unwrap(),
    );
    spec
}

/// A node of four CPUs whose oci class runs in its node VM ([`node_vm_node`]), its view open for one holder, and its node
/// VM, run 39, booked at 2 CPUs and 4 of the least boxes' memory.
fn node_vm_booted(dir: &std::path::Path) -> (Driver, Holder) {
    let runtime: Arc<dyn Runtime> = Arc::new(Routed(hex_api::sim::SimRuntime::default()));
    let driver = Driver::boot(
        NodeId(0),
        &[node_vm_node(four_cpus())],
        &[],
        Budget { owned: 4, ran: 4 },
        Pick::default(),
        dir,
        Runtimes::new([(Class::Oci, runtime)]).unwrap(),
    )
    .unwrap();
    driver.refresh_node(node_vm_node(four_cpus())).unwrap();
    driver.open_view(&dir.join("capacity"), 1).unwrap();
    let mut kiln = Holder::start("kiln-runner".into(), 1, 4, &[]).unwrap();
    let hello = kiln.connect().unwrap();
    let joined = talk(&driver, &mut kiln, vec![hello]);
    assert!(
        matches!(joined.as_slice(), [Event::Synced { agree: true, .. }]),
        "{joined:?}"
    );
    driver
        .book_nodes(&std::collections::BTreeMap::from([(
            hex_api::NodeRun::from_bytes([39; 16]),
            Resources {
                cpu_milli: 2_000,
                mem_bytes: 4 * hex_api::MIN_MEMORY,
                ..Resources::default()
            },
        )]))
        .unwrap();
    (driver, kiln)
}

/// V2 (a) (ROOT, 2026-11-13: the scheduler's ledger booked a node VM beside the oci boxes it hosts, A-5's double count
/// at the scheduler): an oci box in its node's node VM is charged nothing at its host, the node VM's receipt holding it.
/// Do book node VM 39 at 2 of a node's 4 CPUs, then place an oci box of 1 CPU through its NodeVm route. Expect it
/// started, and the node's view holding the node VM's 2 CPUs alone.
#[test]
fn v2_an_oci_box_in_its_node_vm_is_held_once() {
    let dir = tempfile::tempdir().unwrap();
    let (driver, _kiln) = node_vm_booted(dir.path());
    let (d, p) = oci_of(1_000);
    let (_, step) = driver.submit(d, p).unwrap();
    let notices = quiesce(&driver, step);
    assert!(started(&notices), "{notices:?}");
    assert_eq!(driver.view().unwrap().held.cpu_milli, 2_000);
}

/// V2 (ROOT, 2026-11-13: the node VM's route's room is its guest, the host-side pool, so a placement is right the first
/// time): oci boxes are placed within their node VM's guest. Do book node VM 39, its guest admitted at 1.5 CPUs, then
/// place an oci box of 1 CPU, then another. Expect the first started, and the second refused: 2 CPUs past the guest's
/// 1.5.
#[test]
fn v2_oci_boxes_are_placed_within_their_node_vms_guest() {
    let dir = tempfile::tempdir().unwrap();
    let (driver, _kiln) = node_vm_booted(dir.path());
    let (d, p) = oci_of(1_000);
    let (_, step) = driver.submit(d, p).unwrap();
    let notices = quiesce(&driver, step);
    assert!(started(&notices), "{notices:?}");
    let (d, p) = oci_of(1_000);
    let (_, step) = driver.submit(d, p).unwrap();
    assert!(!started(&quiesce(&driver, step)));
}

/// V2 (b) (ROOT, 2026-11-13: a node VM's receipt never reached its node's own admissions): a host box gets only the room
/// past its node's node VM. Do book node VM 39 at 2 of a node's 4 CPUs, then place a process box of 3 CPUs on the host,
/// then one of 2. Expect the first refused, 3 CPUs past the 2 the node VM leaves, and the second started.
#[test]
fn v2_a_host_box_gets_only_the_room_past_its_nodes_node_vm() {
    let dir = tempfile::tempdir().unwrap();
    let (driver, _kiln) = node_vm_booted(dir.path());
    let (d, p) = demand_of(3_000);
    let (_, step) = driver.submit(d, p).unwrap();
    assert!(!started(&quiesce(&driver, step)));
    let (d, p) = demand_of(2_000);
    let (_, step) = driver.submit(d, p).unwrap();
    let notices = quiesce(&driver, step);
    assert!(started(&notices), "{notices:?}");
}
