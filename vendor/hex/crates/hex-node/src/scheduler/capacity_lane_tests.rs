//! The scheduler's capacity lane (joint plan step 13, `docs/design/capacity-view.md` CV6, CV8, CV9; AC-T7.4.1):
//! `hex schedule serve`'s service binds its configured address with the enrollment kept for it, serves the
//! Driver's view to the holders it names alone, hints them after its own words, and takes the person's release.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};

use hex_api::capacity::{Amounts, Close, Event, Holder, Out, ToKeeper};
use hex_core::patience::Patience;
use hex_core::schedule as dto;
use hex_quic::capacity::Link;
use hex_quic::enroll::{self, sim::SimCa, store};
use hex_quic::{Endpoint, PeerId, Refused, Shape};
use hex_testkit::waits::follow::{self, Party};

use super::{CapacityHolder, CapacityLane, Config, Report, Service};

// No recorded operation makes these facts over loopback: each is waited for measuring, following the lane's service by
// its life alone (decision 9), so past the slowest answer seen it goes on, ending with the fact or the link's end. A
// dial and a write wait for their answers with no deadline of the test's: the driver ends a dial nothing answers once
// QUIC's idle timeout passes (RFC 9000 §10.1), and a write cut at a slice's end would leave half a message.
static HEAR: Patience = Patience::measuring("test.hex_node.capacity_lane.hear");
static AWAY: Patience = Patience::measuring("test.hex_node.capacity_lane.away");

const SCHEDULER: &str = "scheduler.capacity.test";
const KILN: &str = "kiln-a.capacity.test";
const KILN_B: &str = "kiln-b.capacity.test";

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

/// A new endpoint `ca` enrolls as `name`, and its dial of the lane at `address`: the lane's peer id, or why not.
fn dialed(ca: &SimCa, address: SocketAddr, name: &str) -> (Endpoint, Result<PeerId, Refused>) {
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
    let peer = end.dial(SCHEDULER, address, None);
    (end, peer)
}

/// A link to the lane at `address` from a new endpoint `ca` enrolls as `name`.
fn linked(ca: &SimCa, address: SocketAddr, name: &str) -> (Endpoint, Link) {
    let (end, peer) = dialed(ca, address, name);
    let link = Link::open(&end, peer.unwrap()).unwrap();
    (end, link)
}

/// The capacity lane as the party of a wait on it, from `lines`, its report's: ended once it said it ended
/// (`hex.capacity event=lane_ended`), its links' end telling a read the rest.
fn lane_party(lines: &Mutex<Vec<String>>) -> Party {
    if lines
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.starts_with("hex.capacity event=lane_ended"))
    {
        Party::Ended
    } else {
        Party::Live
    }
}

/// Says `first` on `link` and each answer to `holder` until neither has more to say, by the life of the lane `lines`
/// reports: what the holder made of it.
fn talk(
    link: &mut Link,
    holder: &mut Holder,
    first: Vec<ToKeeper>,
    lines: &Mutex<Vec<String>>,
) -> Vec<Event> {
    let mut queue = VecDeque::from(first);
    let mut events = Vec::new();
    while let Some(msg) = queue.pop_front() {
        link.say(&msg, None).unwrap();
        let answer = follow::within(
            &HEAR,
            "the lane's next word",
            || lane_party(lines),
            |by| match link.hear(by) {
                Err(Refused::Deadline { .. }) => None,
                heard => Some(heard),
            },
        )
        .unwrap()
        .unwrap();
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

/// AC-T7.4.1, CV6, CV8, CV9, CV11: a scheduler whose configuration names a capacity lane on loopback, its enrollment,
/// and kiln-a and kiln-b alone; kiln-a joins before the scheduler takes any command; kiln-b says hello as kiln-a; the
/// person releases kiln-a while it is connected, again once its link has ended, and a name never seen. Expect the lane
/// bound, kiln-a welcomed with the machine's measured room (a keeper started knows its room before a holder reserves,
/// with no command of Hex's to measure it); kiln-b's hello closed as not authorized and
/// that logged; the release refused as connected, then done and logged as `hex.capacity event=forgotten`, and the name
/// never seen unknown. (A peer the lane doesn't name never reaches it: refused at its handshake,
/// `a_peer_the_lane_doesnt_name_holds_none_of_its_connections`. This scheduler has no backend, so its node runs
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
        holders: [KILN, KILN_B]
            .into_iter()
            .map(|name| CapacityHolder { name: name.into() })
            .collect(),
    };
    let (report, lines) = kept();
    let service = Service::open(&config(dir.path(), Some(lane)), report).unwrap();
    let address = service.capacity_address().unwrap();

    let (_kiln_end, mut link) = linked(&ca, address, KILN);
    let mut kiln = Holder::start(KILN.into(), 1, 4, &[]).unwrap();
    let hello = kiln.connect().unwrap();
    let joined = talk(&mut link, &mut kiln, vec![hello], &lines);
    assert!(
        matches!(joined.as_slice(), [Event::Synced { agree: true, .. }]),
        "{joined:?}"
    );
    let view = kiln.view().unwrap();
    assert!(
        view.room.cpu_milli >= 1000,
        "welcomed before the room was measured: {view:?}"
    );

    let (_b_end, mut posing_link) = linked(&ca, address, KILN_B);
    let mut posing = Holder::start(KILN.into(), 2, 4, &[]).unwrap();
    let hello = posing.connect().unwrap();
    assert_eq!(
        talk(&mut posing_link, &mut posing, vec![hello], &lines),
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
    // A look: the scheduler tells no event as it hears a link end, its release asked again.
    follow::until(
        &AWAY,
        "kiln-a's link heard ended",
        || lane_party(&lines),
        || {
            released = forget(&service, KILN).filter(|f| *f != dto::Forget::Connected);
            released.is_some()
        },
    )
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

/// AC-T7.4.1, CV8, CV9 (goal 13's finding; docs/design/hex-quic.md Q10): a lane that names kiln-a alone holds that
/// holder's two connections (Q8); two peers kiln's CA enrolled but the lane doesn't name dial it and keep their
/// endpoints up; then kiln-a dials and says hello. Expect each unnamed peer refused at its handshake, typed
/// (`Refused::Identity`), so it holds none of the lane's connections, and kiln-a welcomed and synced.
#[test]
fn a_peer_the_lane_doesnt_name_holds_none_of_its_connections() {
    let dir = tempfile::tempdir().unwrap();
    let ca = SimCa::new().unwrap();
    let credentials = dir.path().join("enrollment");
    enrolled_in(&ca, &credentials, SCHEDULER);
    let lane = CapacityLane {
        bind: loopback(),
        credentials,
        holders: vec![CapacityHolder { name: KILN.into() }],
    };
    let (report, lines) = kept();
    let service = Service::open(&config(dir.path(), Some(lane)), report).unwrap();
    let address = service.capacity_address().unwrap();

    let unnamed: Vec<(Endpoint, Result<PeerId, Refused>)> =
        ["mallory-1.capacity.test", "mallory-2.capacity.test"]
            .into_iter()
            .map(|name| dialed(&ca, address, name))
            .collect();
    let (kiln_end, kiln_peer) = dialed(&ca, address, KILN);
    let kiln_peer = kiln_peer
        .unwrap_or_else(|why| panic!("the lane refused kiln-a, the holder it names: {why:?}"));
    let mut link = Link::open(&kiln_end, kiln_peer).unwrap();
    let mut kiln = Holder::start(KILN.into(), 1, 4, &[]).unwrap();
    let hello = kiln.connect().unwrap();
    let joined = talk(&mut link, &mut kiln, vec![hello], &lines);
    assert!(
        matches!(joined.as_slice(), [Event::Synced { agree: true, .. }]),
        "{joined:?}"
    );
    for (_, dial) in &unnamed {
        assert_eq!(dial.as_ref().err(), Some(&Refused::Identity), "{dial:?}");
    }
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
        holders: vec![CapacityHolder { name: KILN.into() }],
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
