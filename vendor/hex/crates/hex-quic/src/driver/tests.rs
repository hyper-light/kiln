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
