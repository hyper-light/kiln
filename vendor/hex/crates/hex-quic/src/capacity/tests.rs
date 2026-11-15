//! The capacity view over QUIC, in process (joint plan step 13, CV9; AC-T7.4.1): a keeper served on its port of an
//! endpoint enrolled with the simulator CA, and holders on endpoints of their own.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Mutex;

use hex_api::capacity::sim::SimView;
use hex_api::capacity::{
    Amounts, Budget, Close, Event, Holder, Host, Out, ReservationId, ToHolder, ToKeeper, Why,
};
use hex_core::patience::Patience;
use hex_testkit::waits::follow::{self, Party};
use hyper_quic::rustls;

use super::*;
use crate::enroll::{self, sim::SimCa};
use crate::{Config, Shape, tls};

// Each read over loopback waits for its own fact, named for it, following the keeper by its life alone (decision 9):
// past the slowest answer seen it goes on, and ends with the answer or the link's end, which the read says itself. A
// dial and a write wait for their answers with no deadline of the test's: the driver ends a dial nothing answers once
// QUIC's idle timeout passes (RFC 9000 §10.1), and a write cut at a slice's end would leave half a message on the link.
/// The keeper's next word to a holder: its answer to what the holder said, or a hint.
static HEAR: Patience = Patience::measuring("test.hex_quic.capacity.hear");
/// The keeper's reset of a connection that broke the protocol.
static RESET: Patience = Patience::measuring("test.hex_quic.capacity.reset");

/// A read's answer within its slice of a following wait, or `None` where the slice ended first.
fn sliced<T>(answer: Result<T, Refused>) -> Option<Result<T, Refused>> {
    match answer {
        Err(Refused::Deadline { .. }) => None,
        answer => Some(answer),
    }
}

/// The keeper's next word on `link`, by the life of the link's connection, whose end ends its hear too.
fn heard(link: &mut Link) -> ToHolder {
    let link = std::cell::RefCell::new(link);
    follow::within(
        &HEAR,
        "the keeper's next word",
        || crate::tests::conn_party(&link.borrow().conn),
        |by| sliced(link.borrow_mut().hear(by)),
    )
    .unwrap()
    .unwrap()
}

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
        link.say(&msg, None).unwrap();
        loop {
            let answer = heard(link);
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
    let a_peer = a_end
        .dial("keeper.capacity.test", node.local_addr(), None)
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
    let b_peer = b_end
        .dial("keeper.capacity.test", node.local_addr(), None)
        .unwrap();
    let mut b_link = Link::open(&b_end, b_peer).unwrap();
    let mut b = Holder::start("kiln-b.capacity.test".into(), 1, 8, &[]).unwrap();
    join(&mut b_link, &mut b);

    let (half, msg) = a.reserve(cpu(500)).unwrap();
    assert_eq!(
        talk(&mut a_link, &mut a, vec![msg]),
        vec![Event::Granted(half)]
    );
    let hint = heard(&mut b_link);
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
    let peer = end
        .dial("keeper-bad.capacity.test", node.local_addr(), None)
        .unwrap();
    let conn = end.open(peer, CAPACITY_PORT).unwrap();
    conn.write_all(&[0, 0, 0, 3, b'{', b'{', b'{'], None)
        .unwrap();
    let mut buf = [0u8; 16];
    let read = follow::within(
        &RESET,
        "the keeper's reset",
        || crate::tests::conn_party(&conn),
        |by| match conn.read(&mut buf, by) {
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => None,
            read => Some(read),
        },
    )
    .unwrap();
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
    let peer = end
        .dial("keeper-auth.capacity.test", node.local_addr(), None)
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
    m_link.say(&theirs, None).unwrap();
    assert_eq!(
        heard(&mut m_link),
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
                && line.ends_with(" why=\"not-authorized\"")),
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

/// The review's E-1, CV7, AC-T7.4.1: a hint is said on the link it is owed to by that link's own writer, never on the
/// thread of the host that owes it, so a holder that stops reading stalls no host and no other holder. Do: a holder
/// joins; its host changes its own admissions and says the hints owed (`Server::hint`) on the test's thread; the holder
/// hears its hint. Expect: the hint said on a thread of the link's own, never the host's.
#[test]
fn e1_a_hint_is_said_by_its_links_own_writer_never_on_its_hosts_thread() {
    let ca = SimCa::new().unwrap();
    // The name `linked` dials.
    let node = endpoint(&ca, "keeper-auth.capacity.test", true);
    let (log, _) = kept();
    let budget = Budget {
        holders: 1,
        reservations: 8,
        name_bytes: 64,
    };
    let server = Server::new(
        SimView::new(budget, cpu(4_000)).unwrap(),
        &named(&["b.e1.test"]),
        log,
    );
    let serving = Arc::clone(&server);
    let acceptor = node.acceptor(CAPACITY_PORT);
    std::thread::spawn(move || serving.serve(acceptor));
    let (_b_end, mut b_link) = linked(&ca, &node, "b.e1.test");
    let mut b = Holder::start("b.e1.test".into(), 1, 8, &[]).unwrap();
    join(&mut b_link, &mut b);
    server.view().lock().unwrap().set_local(cpu(1)).unwrap();
    server.hint().unwrap();
    assert!(matches!(heard(&mut b_link), ToHolder::Hint { .. }));
    let host = std::thread::current().id();
    let said: Vec<(std::thread::ThreadId, bool, usize)> = server
        .links
        .lock()
        .unwrap()
        .values()
        .flat_map(|writer| writer.said_by.lock().unwrap().clone())
        .collect();
    assert!(
        said.iter().any(|(_, hint, _)| *hint),
        "no hint said on the link: {said:?}"
    );
    assert!(
        !said
            .iter()
            .any(|(thread, hint, _)| *hint && *thread == host),
        "a hint said on its host's thread: {said:?}"
    );
}

/// A holder's link filling as its holder stops reading: a fact no recorded operation makes, waited for measuring.
static FILLED: Patience = Patience::measuring("test.hex_quic.capacity.filled");

/// What link `id`'s writer has said whole: its hints, and every message's bytes.
fn said<V>(server: &Server<V>, id: u64) -> (usize, usize) {
    server.links.lock().unwrap().get(&id).map_or((0, 0), |w| {
        let said = w.said_by.lock().unwrap();
        (
            said.iter().filter(|(_, hint, _)| *hint).count(),
            said.iter().map(|(_, _, bytes)| *bytes).sum(),
        )
    })
}

/// The review's E-1, CV7, AC-T7.4.1: a holder that stops reading stalls neither its host's hints nor another holder's
/// link, and the hint owed it waits coalesced until it reads again. Do: holder b joins and stops reading; its host
/// changes its own admissions and says the hints owed, round after round, until b's link holds no more: its writer has
/// said more than the lane's credit (`hex_core::lanes::INITIAL_CREDIT`) past what b read, so a frame waits for room b
/// never grants; then two rounds more; holder a joins, reserves and releases; the host's last round sets a view none
/// before it held; then b reads again. Expect: every round's hints handed on at once; a's words each answered; and b,
/// once it has heard the last view, said at most two hints after its link filled (the one then waiting for room, and
/// the newest), though its host and a owed it more.
#[test]
fn e1_a_holder_that_stops_reading_stalls_no_host_and_no_other_link() {
    let ca = SimCa::new().unwrap();
    // The name `linked` dials.
    let node = endpoint(&ca, "keeper-auth.capacity.test", true);
    let (log, _) = kept();
    let budget = Budget {
        holders: 2,
        reservations: 8,
        name_bytes: 64,
    };
    let server = Server::new(
        SimView::new(budget, cpu(4_000_000)).unwrap(),
        &named(&["a.e1.test", "b.e1.test"]),
        log,
    );
    let serving = Arc::clone(&server);
    let acceptor = node.acceptor(CAPACITY_PORT);
    std::thread::spawn(move || serving.serve(acceptor));
    let (_b_end, mut b_link) = linked(&ca, &node, "b.e1.test");
    let mut b = Holder::start("b.e1.test".into(), 1, 8, &[]).unwrap();
    join(&mut b_link, &mut b);
    let b_id = *server.links.lock().unwrap().keys().next().unwrap();
    // All b has read: every word said to it as it joined.
    let (_, read) = said(&server, b_id);
    let mut local = 0u64;
    let round = |local: &mut u64| {
        *local += 1;
        server
            .view()
            .lock()
            .unwrap()
            .set_local(cpu(*local))
            .unwrap();
        server.hint().unwrap();
    };
    follow::until(
        &FILLED,
        "b's link holding no more, b not reading",
        || {
            if server.links.lock().unwrap().contains_key(&b_id) {
                Party::Live
            } else {
                Party::Ended
            }
        },
        || {
            round(&mut local);
            let (_, bytes) = said(&server, b_id);
            u64::try_from(bytes - read).unwrap() > hex_core::lanes::INITIAL_CREDIT
        },
    )
    .unwrap();
    let ((filled, _), filled_at) = (said(&server, b_id), local);
    round(&mut local);
    round(&mut local);
    let (_a_end, mut a_link) = linked(&ca, &node, "a.e1.test");
    let mut a = Holder::start("a.e1.test".into(), 1, 8, &[]).unwrap();
    join(&mut a_link, &mut a);
    let (id, reserve) = a.reserve(cpu(1_000)).unwrap();
    let granted = talk(&mut a_link, &mut a, vec![reserve]);
    assert!(
        matches!(granted.as_slice(), [Event::Granted(granted)] if *granted == id),
        "{granted:?}"
    );
    let mut out = Out::default();
    a.release(id, &mut out);
    let released = talk(&mut a_link, &mut a, out.send);
    assert!(
        matches!(released.as_slice(), [Event::Released(released)] if *released == id),
        "{released:?}"
    );
    round(&mut local);
    // Each host round since b's link filled changed the view, and so did a's reserve and its release.
    let owed = usize::try_from(local - filled_at + 2).unwrap();
    loop {
        if let ToHolder::Hint { view, .. } = heard(&mut b_link)
            && view.held.cpu_milli == local
        {
            break;
        }
    }
    let said = said(&server, b_id).0 - filled;
    assert!(
        said <= 2 && said < owed,
        "{said} hints said to b after its link filled, of the {owed} views owed it since"
    );
}

/// The lane's refused line (CLAUDE.md §10; ROOT, 2026-11-14): its values JSON strings or whole numbers, so a holder's
/// name can't split it. Do: form it for a name holding a quote, a backslash and a newline. Expect: one line, the name a
/// JSON string, the peer and the link whole numbers.
#[test]
fn a_refused_holders_line_is_one_line_its_name_a_json_string() {
    assert_eq!(
        super::refused_line(7, 3, "mallory \"x\" \\\nhex.capacity event=forged"),
        "hex.capacity event=refused peer=7 link=3 holder=\"mallory \\\"x\\\" \\\\\\nhex.capacity event=forged\" \
         why=\"not-authorized\""
    );
}
