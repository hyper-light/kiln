//! The scheduler's capacity lane (joint plan step 13, `docs/design/capacity-view.md` CV6, CV8, CV9; AC-T7.4.1):
//! `hex schedule serve`'s service binds its configured address with the enrollment kept for it, serves the
//! Driver's view to the holders it names alone, hints them after its own words, and takes the person's release.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};

use hex_api::capacity::{Amounts, Close, Event, Holder, Out, ToHolder, ToKeeper};
use hex_core::patience::Patience;
use hex_core::schedule as dto;
use hex_quic::capacity::Link;
use hex_quic::enroll::{self, sim::SimCa, store};
use hex_quic::{Endpoint, Shape};
use hex_testkit::waits::{until, within};

use super::{CapacityHolder, CapacityLane, Config, Report, Service};

// No recorded operation makes these facts over loopback: each is waited for measuring (hex_testkit::waits).
static DIAL: Patience = Patience::measuring("test.hex_node.capacity_lane.dial");
static SAY: Patience = Patience::measuring("test.hex_node.capacity_lane.say");
static HEAR: Patience = Patience::measuring("test.hex_node.capacity_lane.hear");
static AWAY: Patience = Patience::measuring("test.hex_node.capacity_lane.away");

const SCHEDULER: &str = "scheduler.capacity.test";
const KILN: &str = "kiln-a.capacity.test";

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

/// `name`'s enrollment in `dir`, as `hex node enroll` begins it and kiln's CA (here, `ca`) answers its request.
fn enrolled_in(ca: &SimCa, dir: &Path, name: &str) {
    store::begin(dir, name).unwrap();
    let request = store::request(dir, b"grant".to_vec()).unwrap();
    ca.admit(name, b"grant");
    let issued = enroll::Ca::enroll(ca, &request).unwrap();
    store::finish(dir, &issued).unwrap();
}

fn config(dir: &Path, capacity: Option<CapacityLane>) -> Config {
    Config {
        cluster: "capacity-lane".into(),
        node: 0,
        state: dir.join("state"),
        socket: dir.join("s.sock"),
        nodes: Vec::new(),
        models: Vec::new(),
        backends: Vec::new(),
        peers: Vec::new(),
        capacity,
    }
}

/// A report that keeps every line it is told.
fn kept() -> (Report, Arc<Mutex<Vec<String>>>) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    let report: Report = Arc::new(move |_, line| sink.lock().unwrap().push(line.to_owned()));
    (report, lines)
}

/// A link to the lane at `address` from a new endpoint `ca` enrolls as `name`.
fn linked(ca: &SimCa, address: SocketAddr, name: &str) -> (Endpoint, Link) {
    ca.admit(name, name.as_bytes());
    let end = Endpoint::start(hex_quic::Config {
        credentials: enroll::enroll(ca, name, name.as_bytes().to_vec()).unwrap(),
        bind: loopback(),
        listen: false,
        serves: Vec::new(),
        shape: Shape {
            connections: 1,
            lanes: 2,
        },
    })
    .unwrap();
    let peer = within(&DIAL, |by| end.dial(SCHEDULER, address, by)).unwrap();
    let link = Link::open(&end, peer).unwrap();
    (end, link)
}

/// Says `first` on `link` and each answer to `holder` until neither has more to say: what the holder made of it.
fn talk(link: &mut Link, holder: &mut Holder, first: Vec<ToKeeper>) -> Vec<Event> {
    let mut queue = VecDeque::from(first);
    let mut events = Vec::new();
    while let Some(msg) = queue.pop_front() {
        within(&SAY, |by| link.say(&msg, by)).unwrap();
        let answer = within(&HEAR, |by| link.hear(by)).unwrap();
        let mut out = Out::default();
        holder.hear(answer, &mut out);
        queue.extend(out.send);
        events.extend(out.events);
    }
    events
}

fn forget(service: &Service, holder: &str) -> Option<dto::Forget> {
    service
        .command(dto::Command::Forget {
            holder: holder.into(),
        })
        .unwrap()
        .forget
}

/// AC-T7.4.1, CV6, CV8, CV9: a scheduler whose configuration names a capacity lane on loopback, its enrollment, and
/// kiln-a alone; kiln-a joins and the scheduler refreshes; a peer kiln's CA enrolled but the lane doesn't name says
/// hello; the person releases kiln-a while it is connected, again once its link has ended, and a name never seen.
/// Expect the lane bound, kiln-a welcomed and hinted the measured room after the scheduler's own word; the other peer
/// closed as not authorized and that logged; the release refused as connected, then done and logged as
/// `hex.capacity event=forgotten`, and the name never seen unknown. (This scheduler has no backend, so its node runs
/// nothing and keeps no reservation of a holder's, CV8: the grant through the node's view is
/// `placement::view::tests::a_holder_reaches_the_nodes_keeper_over_quic`.)
#[test]
fn the_scheduler_serves_its_capacity_view_to_the_holders_it_names_alone() {
    let dir = tempfile::tempdir().unwrap();
    let ca = SimCa::new().unwrap();
    let credentials = dir.path().join("enrollment");
    enrolled_in(&ca, &credentials, SCHEDULER);
    let lane = CapacityLane {
        bind: loopback(),
        credentials,
        holders: vec![CapacityHolder {
            name: KILN.into(),
            user: None,
        }],
    };
    let (report, lines) = kept();
    let service = Service::open(&config(dir.path(), Some(lane)), report).unwrap();
    let address = service.capacity_address().unwrap();

    let (_kiln_end, mut link) = linked(&ca, address, KILN);
    let mut kiln = Holder::start(KILN.into(), 1, 4, &[]).unwrap();
    let hello = kiln.connect().unwrap();
    let joined = talk(&mut link, &mut kiln, vec![hello]);
    assert!(
        matches!(joined.as_slice(), [Event::Synced { agree: true, .. }]),
        "{joined:?}"
    );
    service.command(dto::Command::Retry).unwrap();
    let hint = within(&HEAR, |by| link.hear(by)).unwrap();
    let ToHolder::Hint { view, .. } = hint.clone() else {
        panic!("no hint after the scheduler measured its room: {hint:?}");
    };
    assert!(view.room.cpu_milli >= 1000, "{view:?}");
    let mut out = Out::default();
    kiln.hear(hint, &mut out);
    assert_eq!(kiln.view().map(|v| v.room), Some(view.room));

    let (_other_end, mut other) = linked(&ca, address, "mallory.capacity.test");
    let mut mallory = Holder::start("mallory.capacity.test".into(), 1, 4, &[]).unwrap();
    let hello = mallory.connect().unwrap();
    assert_eq!(
        talk(&mut other, &mut mallory, vec![hello]),
        vec![Event::Closed(Close::NotAuthorized)]
    );
    assert!(
        lines
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with("hex.capacity event=refused ")),
        "{lines:?}"
    );

    assert_eq!(forget(&service, KILN), Some(dto::Forget::Connected));
    drop(link);
    let mut released = None;
    until(&AWAY, "kiln-a's link heard ended", || {
        released = forget(&service, KILN).filter(|f| *f != dto::Forget::Connected);
        released.is_some()
    })
    .unwrap();
    assert_eq!(
        released,
        Some(dto::Forget::Released {
            holder: KILN.into(),
            epoch: 1,
            count: 0,
            sum: Amounts::ZERO,
        })
    );
    assert!(
        lines.lock().unwrap().iter().any(|line| line
            == "hex.capacity event=forgotten holder=\"kiln-a.capacity.test\" epoch=1 count=0 cpu_milli=0 \
                mem_bytes=0 disk_bytes=0 gpus=0"),
        "{lines:?}"
    );
    assert_eq!(forget(&service, "nobody.test"), Some(dto::Forget::Unknown));
    service.shutdown().unwrap();
}

/// AC-T7.4.1, CV9: a scheduler whose configuration names no capacity lane, then one whose lane's enrollment has
/// begun but kiln's CA hasn't answered. Expect the first to bind nothing for one and to refuse the person's release,
/// typed; the second refused at its open, typed, before anything is bound.
#[test]
fn without_a_lane_nothing_is_bound_and_a_release_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (report, _) = kept();
    let service = Service::open(&config(&dir.path().join("none"), None), report).unwrap();
    assert_eq!(service.capacity_address(), None);
    let refused = service
        .command(dto::Command::Forget {
            holder: KILN.into(),
        })
        .unwrap_err();
    assert!(
        matches!(
            refused,
            hex_api::Refusal::Invalid {
                what: "capacity view",
                ..
            }
        ),
        "{refused:?}"
    );
    service.shutdown().unwrap();

    let begun = dir.path().join("begun");
    store::begin(&begun, SCHEDULER).unwrap();
    let lane = CapacityLane {
        bind: loopback(),
        credentials: begun,
        holders: vec![CapacityHolder {
            name: KILN.into(),
            user: None,
        }],
    };
    let (report, _) = kept();
    let opened = Service::open(&config(&dir.path().join("some"), Some(lane)), report);
    assert!(
        matches!(
            opened,
            Err(hex_api::Refusal::Invalid {
                what: "capacity lane",
                ..
            })
        ),
        "{:?}",
        opened.err()
    );
    assert!(
        !dir.path().join("some/state").exists(),
        "the scheduler opened its state before refusing its lane"
    );
}
