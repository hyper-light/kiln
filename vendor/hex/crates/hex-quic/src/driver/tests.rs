//! The driver's directory (docs/design/hex-quic.md Q5, Q10; AC-T7.4.1): the peer a handshake names, and whether the
//! endpoint admits it.

use std::collections::BTreeMap;

use hyper_transport::{Authenticated, Directory, PeerId};

use super::{Names, peer_id};
use crate::Admits;
use crate::enroll::{self, sim::SimCa};

const KILN: &str = "kiln-a.admits.test";
const MALLORY: &str = "mallory.admits.test";

/// The certificate kiln's CA (here, `ca`) issues `name`, as DER.
fn certificate(ca: &SimCa, name: &str) -> Vec<u8> {
    ca.admit(name, name.as_bytes());
    let credentials = enroll::enroll(ca, name, name.as_bytes().to_vec()).unwrap();
    credentials.chain[0].as_ref().to_vec()
}

/// The peer `names` says a handshake presenting `certificate` is, as hyper-transport asks it once the handshake
/// completes; `dialed` is the peer this side dialed, if it did.
fn identify(names: &mut Names, certificate: &[u8], dialed: Option<PeerId>) -> Option<PeerId> {
    names
        .identify(&Authenticated {
            certificate: Some(certificate),
            protocol: crate::tls::ALPN,
            dialed,
        })
        .map(|(peer, _)| peer)
}

/// AC-T7.4.1, Q10: an endpoint that admits kiln-a alone, then one that admits every peer kiln's CA enrolled. Do:
/// identify a connection kiln-a opened, one mallory (enrolled too) opened, this side's dial of mallory, and a dial of
/// mallory that kiln-a answered. Expect, where it names kiln-a alone: kiln-a named, mallory's own connection named no
/// peer (hyper-transport refuses it Identity, charged to no identity), the dial of mallory named mallory (a dial names
/// its peer itself, Q5), and the dial answered by another refused, a name kept for the driver's links report only for
/// a handshake admitted (Q12); where it admits every enrolled peer, mallory's connection named too.
#[test]
fn an_endpoint_that_names_its_peers_admits_their_connections_alone() {
    let ca = SimCa::new().unwrap();
    let kiln = certificate(&ca, KILN);
    let mallory = certificate(&ca, MALLORY);
    let mut named = Names {
        dialing: BTreeMap::new(),
        admits: Admits::Named([KILN.to_owned()].into()),
        identified: BTreeMap::new(),
    };
    assert_eq!(identify(&mut named, &kiln, None), Some(peer_id(KILN)));
    assert_eq!(identify(&mut named, &mallory, None), None);
    assert_eq!(
        named.identified,
        BTreeMap::from([(peer_id(KILN), KILN.to_owned())])
    );
    assert_eq!(
        identify(&mut named, &mallory, Some(peer_id(MALLORY))),
        Some(peer_id(MALLORY))
    );
    assert_eq!(identify(&mut named, &kiln, Some(peer_id(MALLORY))), None);
    let mut enrolled = Names {
        dialing: BTreeMap::new(),
        admits: Admits::Enrolled,
        identified: BTreeMap::new(),
    };
    assert_eq!(
        identify(&mut enrolled, &mallory, None),
        Some(peer_id(MALLORY))
    );
    assert_eq!(identify(&mut enrolled, &kiln, Some(peer_id(MALLORY))), None);
}

/// The review's E-1 (a holder that stops reading, then reads again), AC-T7.4.1, Q3: a lane whose owner reads faster than
/// the lane carries holds back the Credit its reads grant, and keeps its peer. Do: hold back on one lane what the driver
/// holds back while QUIC hasn't taken the lane's last frame as its owner reads frame after frame: a Credit each, of
/// each frame's bytes, two more than the lane holds back of other frames ([`super::WAITING`]). Expect: none refused, and
/// what the lane holds back grants the peer every byte read.
#[test]
fn a_lane_its_owner_reads_faster_than_it_carries_holds_back_its_credit_and_keeps_its_peer() {
    use std::collections::VecDeque;

    use hex_core::lanes::{DATA_BOUND, Kind};

    use super::{WAITING, control_frame, hold_back};

    let frame = u64::try_from(DATA_BOUND).unwrap();
    let reads = WAITING + 2;
    let mut waiting = VecDeque::new();
    for read in 0..reads {
        assert_eq!(
            hold_back(
                &mut waiting,
                Kind::Credit,
                control_frame(Kind::Credit, frame.to_be_bytes().to_vec())
            ),
            Ok(()),
            "the Credit of read {read} refused: its peer would be refused Busy and the connection closed"
        );
    }
    let granted: u64 = waiting
        .iter()
        .map(|(kind, payload)| {
            assert_eq!(*kind, Kind::Credit);
            u64::from_be_bytes(payload.bytes().try_into().unwrap())
        })
        .sum();
    assert_eq!(granted, frame * u64::try_from(reads).unwrap());
}

/// The review's E-1 path, AC-T7.4.1, Q3: a Credit held back takes in the later grants of its own connection alone. Do:
/// on one lane, hold back a Credit, the connection's End and a second Credit (its owner still reads once its half has
/// ended); then its Reset, the next connection's Accept, and a Credit of that one. Expect: the first connection's two
/// Credits one, before its End; the next connection's Credit apart, after its Accept, never added to the first's.
#[test]
fn a_held_back_credit_takes_in_its_own_connections_grants_alone() {
    use std::collections::VecDeque;

    use hex_core::lanes::Kind;

    use super::{control_frame, hold_back};

    let credit = |bytes: u64| bytes.to_be_bytes().to_vec();
    let mut waiting = VecDeque::new();
    for (kind, payload) in [
        (Kind::Credit, credit(3)),
        (Kind::End, Vec::new()),
        (Kind::Credit, credit(4)),
        (Kind::Reset, vec![0]),
        (Kind::Accept, 9u64.to_be_bytes().to_vec()),
        (Kind::Credit, credit(5)),
    ] {
        hold_back(&mut waiting, kind, control_frame(kind, payload)).unwrap();
    }
    assert_eq!(
        waiting
            .iter()
            .map(|(kind, payload)| (*kind, payload.bytes().to_vec()))
            .collect::<Vec<_>>(),
        vec![
            (Kind::Credit, credit(7)),
            (Kind::End, Vec::new()),
            (Kind::Reset, vec![0]),
            (Kind::Accept, 9u64.to_be_bytes().to_vec()),
            (Kind::Credit, credit(5)),
        ]
    );
}
