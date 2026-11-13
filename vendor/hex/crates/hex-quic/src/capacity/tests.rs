//! The capacity view over QUIC, in process (joint plan step 13, CV9; AC-T7.4.1): a keeper served on its port of an
//! endpoint enrolled with the simulator CA, and holders on endpoints of their own.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Mutex;

use hex_api::capacity::sim::SimView;
use hex_api::capacity::{
    Amounts, Budget, Close, Event, Holder, Out, ReservationId, ToHolder, ToKeeper, Why,
};
use hex_core::patience::Patience;
use hex_testkit::waits::within;
use hyper_quic::rustls;

use super::*;
use crate::enroll::{self, sim::SimCa};
use crate::{Config, Shape, tls};

// Each wait over loopback is for its own fact, named for it. No census over QUIC has recorded one yet, so each
// measures, for the fact alone, its answers recorded for the census that will seed it (hex-testkit's rule).
/// A dial's answer: its handshake, taken or refused, and the keeper's endpoint's Welcome.
static DIAL: Patience = Patience::measuring("test.hex_quic.capacity.dial");
/// Room on the keeper's port for what a holder says: the keeper's grants.
static SAY: Patience = Patience::measuring("test.hex_quic.capacity.say");
/// The keeper's next word to a holder: its answer to what the holder said, or a hint.
static HEAR: Patience = Patience::measuring("test.hex_quic.capacity.hear");
/// The keeper's reset of a connection that broke the protocol.
static RESET: Patience = Patience::measuring("test.hex_quic.capacity.reset");

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

fn enrolled(ca: &SimCa, name: &str) -> tls::Credentials {
    ca.admit(name, name.as_bytes());
    enroll::enroll(ca, name, name.as_bytes().to_vec()).unwrap()
}

fn endpoint(ca: &SimCa, name: &str, listen: bool) -> Endpoint {
    Endpoint::start(Config {
        credentials: enrolled(ca, name),
        bind: loopback(),
        listen,
        serves: if listen {
            vec![CAPACITY_PORT]
        } else {
            Vec::new()
        },
        shape: Shape {
            connections: 4,
            lanes: 8,
        },
    })
    .unwrap()
}

/// The holders a keeper serves, by the names their certificates carry.
fn named(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// A log that keeps what it is told.
fn kept() -> (Log, Arc<Mutex<Vec<String>>>) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    let log: Log = Arc::new(move |line: &str| sink.lock().unwrap().push(line.to_owned()));
    (log, lines)
}

fn cpu(milli: u64) -> Amounts {
    Amounts {
        cpu_milli: milli,
        ..Amounts::ZERO
    }
}

/// Says `first` on `link`, and each answer to `holder`, until neither has more to say: hints that come between are
/// heard too.
fn talk(link: &mut Link, holder: &mut Holder, first: Vec<ToKeeper>) -> Vec<Event> {
    let mut queue = VecDeque::from(first);
    let mut events = Vec::new();
    while let Some(msg) = queue.pop_front() {
        within(&SAY, |by| link.say(&msg, by)).unwrap();
        loop {
            let answer = within(&HEAR, |by| link.hear(by)).unwrap();
            let hint = matches!(answer, ToHolder::Hint { .. });
            let mut out = Out::default();
            holder.hear(answer, &mut out);
            queue.extend(out.send);
            events.extend(out.events);
            if !hint {
                break;
            }
        }
    }
    events
}

fn join(link: &mut Link, holder: &mut Holder) {
    let hello = holder.connect().unwrap();
    let events = talk(link, holder, vec![hello]);
    assert!(
        matches!(events.as_slice(), [Event::Synced { agree: true, .. }]),
        "{events:?}"
    );
}

/// AC-T7.4.1, CV9 (and the plan's AES-256-GCM only): two holders on endpoints of their own reach a keeper served on
/// its port over QUIC; one reserves 3 of the room's 4 CPUs and then half of one, the other asks one; the first's
/// connection is dropped and the other asks again, then the first opens a new one and rejoins; expect the link to
/// have negotiated TLS13_AES_256_GCM_SHA384, the grants, a hint to the other holder with the view, its ask refused
/// for room both times (a dropped link releasing nothing), and the rejoin's sync agreeing with the keeper's books.
#[test]
fn holders_reserve_through_their_keeper_over_quic() {
    let ca = SimCa::new().unwrap();
    let node = endpoint(&ca, "keeper.capacity.test", true);
    let budget = Budget {
        holders: 2,
        reservations: 8,
        name_bytes: 64,
    };
    let (log, _) = kept();
    let server = Server::new(
        SimView::new(budget, cpu(4000)).unwrap(),
        &named(&["kiln-a.capacity.test", "kiln-b.capacity.test"]),
        log,
    );
    let serving = {
        let server = Arc::clone(&server);
        let acceptor = node.acceptor(CAPACITY_PORT);
        std::thread::spawn(move || server.serve(acceptor))
    };

    let a_end = endpoint(&ca, "kiln-a.capacity.test", false);
    let a_peer = within(&DIAL, |by| {
        a_end.dial("keeper.capacity.test", node.local_addr(), by)
    })
    .unwrap();
    let (suite, _) = a_end.negotiated(a_peer).unwrap().unwrap();
    assert_eq!(suite, rustls::CipherSuite::TLS13_AES_256_GCM_SHA384);
    let mut a_link = Link::open(&a_end, a_peer).unwrap();
    let mut a = Holder::start("kiln-a.capacity.test".into(), 1, 8, &[]).unwrap();
    join(&mut a_link, &mut a);
    let (big, msg) = a.reserve(cpu(3000)).unwrap();
    assert_eq!(
        talk(&mut a_link, &mut a, vec![msg]),
        vec![Event::Granted(big)]
    );

    let b_end = endpoint(&ca, "kiln-b.capacity.test", false);
    let b_peer = within(&DIAL, |by| {
        b_end.dial("keeper.capacity.test", node.local_addr(), by)
    })
    .unwrap();
    let mut b_link = Link::open(&b_end, b_peer).unwrap();
    let mut b = Holder::start("kiln-b.capacity.test".into(), 1, 8, &[]).unwrap();
    join(&mut b_link, &mut b);

    let (half, msg) = a.reserve(cpu(500)).unwrap();
    assert_eq!(
        talk(&mut a_link, &mut a, vec![msg]),
        vec![Event::Granted(half)]
    );
    let hint = within(&HEAR, |by| b_link.hear(by)).unwrap();
    assert!(matches!(hint, ToHolder::Hint { .. }), "{hint:?}");
    let mut out = Out::default();
    b.hear(hint, &mut out);
    assert_eq!(b.view().map(|view| view.held), Some(cpu(3500)));

    let (first, msg) = b.reserve(cpu(1000)).unwrap();
    assert_eq!(
        talk(&mut b_link, &mut b, vec![msg]),
        vec![Event::Refused(first, Why::Room)]
    );

    drop(a_link);
    let mut out = Out::default();
    a.link_down(&mut out);
    let (second, msg) = b.reserve(cpu(1000)).unwrap();
    assert_eq!(
        talk(&mut b_link, &mut b, vec![msg]),
        vec![Event::Refused(second, Why::Room)]
    );

    let mut a_link = Link::open(&a_end, a_peer).unwrap();
    join(&mut a_link, &mut a);
    let books = server
        .view()
        .lock()
        .unwrap()
        .held_by("kiln-a.capacity.test")
        .unwrap();
    assert_eq!(books, a.held().collect::<Vec<_>>());

    drop((a_link, b_link, a_end, b_end, node));
    assert!(matches!(serving.join().unwrap(), Refused::Ended));
}

/// AC-T7.4.1, CV9, CLAUDE.md §3: say bytes that aren't a capacity message on the keeper's port; expect the keeper
/// to reset that connection as breaking the protocol, and to serve a new one.
#[test]
fn a_connection_that_breaks_the_protocol_is_reset() {
    let ca = SimCa::new().unwrap();
    let node = endpoint(&ca, "keeper-bad.capacity.test", true);
    let budget = Budget {
        holders: 1,
        reservations: 8,
        name_bytes: 64,
    };
    let (log, _) = kept();
    let server = Server::new(
        SimView::new(budget, cpu(4000)).unwrap(),
        &named(&["kiln-bad.capacity.test"]),
        log,
    );
    {
        let server = Arc::clone(&server);
        let acceptor = node.acceptor(CAPACITY_PORT);
        std::thread::spawn(move || server.serve(acceptor));
    }
    let end = endpoint(&ca, "kiln-bad.capacity.test", false);
    let peer = within(&DIAL, |by| {
        end.dial("keeper-bad.capacity.test", node.local_addr(), by)
    })
    .unwrap();
    let conn = end.open(peer, CAPACITY_PORT).unwrap();
    within(&SAY, |by| {
        conn.write_all(&[0, 0, 0, 3, b'{', b'{', b'{'], by)
    })
    .unwrap();
    let mut buf = [0u8; 16];
    let read = within(&RESET, |by| conn.read(&mut buf, by));
    assert!(
        read.is_err() || read.as_ref().is_ok_and(|n| *n == 0),
        "{read:?}"
    );
    let mut link = Link::open(&end, peer).unwrap();
    let mut holder = Holder::start("kiln-bad.capacity.test".into(), 1, 8, &[]).unwrap();
    join(&mut link, &mut holder);
}

/// A link to the keeper at `node` from a new endpoint enrolled as `name`.
fn linked(ca: &SimCa, node: &Endpoint, name: &str) -> (Endpoint, Link) {
    let end = endpoint(ca, name, false);
    let peer = within(&DIAL, |by| {
        end.dial("keeper-auth.capacity.test", node.local_addr(), by)
    })
    .unwrap();
    let link = Link::open(&end, peer).unwrap();
    (end, link)
}

/// AC-T7.4.1, CV8, CV9: a keeper that authorizes kiln-a and kiln-b alone; kiln-a joins and reserves; a peer kiln's CA
/// enrolled but the keeper doesn't authorize says hello, then a reserve under kiln-a's fence; kiln-b says hello as
/// kiln-a. Expect kiln-a served; the other peer's hello and reserve closed as not authorized, typed (its holder told
/// Closed(NotAuthorized)), kiln-a's books unchanged, and one refusal logged for it; kiln-b's hello closed the same way
/// and logged.
#[test]
fn only_the_holders_its_host_authorizes_are_served() {
    let ca = SimCa::new().unwrap();
    let node = endpoint(&ca, "keeper-auth.capacity.test", true);
    let budget = Budget {
        holders: 2,
        reservations: 8,
        name_bytes: 64,
    };
    let (log, lines) = kept();
    let server = Server::new(
        SimView::new(budget, cpu(4000)).unwrap(),
        &named(&["kiln-a.capacity.test", "kiln-b.capacity.test"]),
        log,
    );
    {
        let server = Arc::clone(&server);
        let acceptor = node.acceptor(CAPACITY_PORT);
        std::thread::spawn(move || server.serve(acceptor));
    }
    let (_a_end, mut a_link) = linked(&ca, &node, "kiln-a.capacity.test");
    let mut a = Holder::start("kiln-a.capacity.test".into(), 1, 8, &[]).unwrap();
    join(&mut a_link, &mut a);
    let (run, msg) = a.reserve(cpu(1000)).unwrap();
    let ToKeeper::Reserve { fence, .. } = msg.clone() else {
        panic!("{msg:?}");
    };
    assert_eq!(
        talk(&mut a_link, &mut a, vec![msg]),
        vec![Event::Granted(run)]
    );

    let (_m_end, mut m_link) = linked(&ca, &node, "mallory.capacity.test");
    let mut mallory = Holder::start("mallory.capacity.test".into(), 1, 8, &[]).unwrap();
    let hello = mallory.connect().unwrap();
    assert_eq!(
        talk(&mut m_link, &mut mallory, vec![hello]),
        vec![Event::Closed(Close::NotAuthorized)]
    );
    let theirs = ToKeeper::Reserve {
        fence,
        id: ReservationId { epoch: 1, seq: 7 },
        amount: cpu(3000),
    };
    within(&SAY, |by| m_link.say(&theirs, by)).unwrap();
    assert_eq!(
        within(&HEAR, |by| m_link.hear(by)).unwrap(),
        ToHolder::Closed {
            rejected: Some(fence),
            why: Close::NotAuthorized
        }
    );
    let books = server
        .view()
        .lock()
        .unwrap()
        .held_by("kiln-a.capacity.test")
        .unwrap();
    assert_eq!(books, a.held().collect::<Vec<_>>());

    let (_b_end, mut b_link) = linked(&ca, &node, "kiln-b.capacity.test");
    let mut posing = Holder::start("kiln-a.capacity.test".into(), 2, 8, &[]).unwrap();
    let hello = posing.connect().unwrap();
    assert_eq!(
        talk(&mut b_link, &mut posing, vec![hello]),
        vec![Event::Closed(Close::NotAuthorized)]
    );
    let lines = lines.lock().unwrap();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with("hex.capacity event=refused ")
                && line.ends_with(" why=not-authorized")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("holder=\"mallory.capacity.test\""))
            && lines
                .iter()
                .any(|line| line.contains("holder=\"kiln-a.capacity.test\"")),
        "{lines:?}"
    );
}
