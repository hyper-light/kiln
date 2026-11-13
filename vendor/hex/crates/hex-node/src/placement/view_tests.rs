//! The node's capacity view (joint plan step 13; AC-T7.4.1): its shell over the node's record files held
//! to the conformance suite, and the node's own admission and a holder's reserve checking one view.

use std::collections::VecDeque;
use std::sync::Arc;

use hex_api::capacity::{
    Amounts, CapacityView, Event, Holder, Host, Out, Read, Scope, ToHolder, ToKeeper, Why,
    conformance,
};
use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Refusal, Runtime};
use hex_sim::index::Pick;
use hex_sim::model::{Ask, Class, Demand, Kind, NodeId, NodeSpec, Resources};
use hex_sim::peer::{Budget, Stop};

use super::View;
use crate::placement::{Driver, Notice, Payload, Runtimes, Step};

const HOLDERS: usize = 2;
const RAN: usize = 8;

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
        let mut view = View::open(dir.path(), HOLDERS, RAN).map_err(refusal)?;
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
    fn sample(&mut self, room: Amounts, used: &[(u64, Read)]) -> Result<(), Refusal> {
        self.room = room;
        self.view.sample(room, used);
        Ok(())
    }

    fn scopes(&mut self) -> Result<Vec<(u64, Scope)>, Refusal> {
        Ok(self
            .view
            .scopes()
            .into_iter()
            .map(|(link, _, scope)| (link, scope))
            .collect())
    }

    fn set_local(&mut self, local: Amounts) -> Result<(), Refusal> {
        self.local = local;
        Ok(())
    }

    fn restart(&mut self) -> Result<(), Refusal> {
        self.view = View::open(self.dir.path(), HOLDERS, RAN).map_err(refusal)?;
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
    assert_eq!(driver.rebook(&[(vm, holding(1000))]).unwrap(), 1);
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
    driver.rebook(&[(vm, holding(1000))]).unwrap();
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
    driver.rebook(&[(vm, holding(5000))]).unwrap();
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
    driver.rebook(&[(vm, holding(1000))]).unwrap();
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
    use hex_quic::{Config, Endpoint, Shape};
    use hex_testkit::waits::within;

    // Each wait over loopback is for its own fact, named for it. No census over QUIC has recorded one yet, so each
    // measures, for the fact alone, its answers recorded for the census that will seed it (hex-testkit's rule).
    /// A dial's answer: its handshake and the node's Welcome.
    static DIAL: Patience = Patience::measuring("test.hex_node.capacity.dial");
    /// Room on the keeper's port for what the holder says: the keeper's grants.
    static SAY: Patience = Patience::measuring("test.hex_node.capacity.say");
    /// The keeper's next word to the holder: its answer to what it said, or a hint.
    static HEAR: Patience = Patience::measuring("test.hex_node.capacity.hear");

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
    {
        let server = Arc::clone(&server);
        let acceptor = node.acceptor(CAPACITY_PORT);
        std::thread::spawn(move || server.serve(acceptor));
    }
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
    let peer = within(&DIAL, |by| {
        kiln_end.dial("node.capacity.test", node.local_addr(), by)
    })
    .unwrap();
    let mut link = Link::open(&kiln_end, peer).unwrap();
    let mut kiln = Holder::start("kiln.capacity.test".into(), 1, 4, &[]).unwrap();
    let mut said = VecDeque::from(vec![kiln.connect().unwrap()]);
    let mut events = Vec::new();
    let mut converse = |link: &mut Link, kiln: &mut Holder, said: &mut VecDeque<ToKeeper>| {
        while let Some(msg) = said.pop_front() {
            within(&SAY, |by| link.say(&msg, by)).unwrap();
            let mut out = Out::default();
            kiln.hear(within(&HEAR, |by| link.hear(by)).unwrap(), &mut out);
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
    let ToHolder::Hint { view, .. } = within(&HEAR, |by| link.hear(by)).unwrap() else {
        panic!("no hint");
    };
    assert_eq!((view.room.cpu_milli, view.held.cpu_milli), (4000, 4000));
}
