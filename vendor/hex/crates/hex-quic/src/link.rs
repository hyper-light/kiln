//! The node agent's client over QUIC (docs/design/hex-quic.md Q3): `hex_client::guest`'s client, unchanged, over a
//! link whose dials are connections to the node's ports, each the owner's end of a local socket ([`crate::bridge`]),
//! so the calls, a program's streams and staging travel as they do to a guest's node.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use hex_api::Refusal;
use hex_client::guest::{self, DuplexOwner};

use crate::{Endpoint, PeerId, Refused, bridge};

/// The link to one node.
pub struct Link {
    endpoint: Arc<Endpoint>,
    peer: PeerId,
}

impl Link {
    /// The link to `peer` over `endpoint`, connected already ([`Endpoint::dial`]): what staging dials the node's files
    /// port through (`hex_client::guest::stage`).
    pub fn new(endpoint: Arc<Endpoint>, peer: PeerId) -> Self {
        Self { endpoint, peer }
    }
}

fn refusal(op: &'static str) -> impl Fn(Refused) -> Refusal {
    move |e| Refusal::Os {
        op,
        why: e.to_string(),
    }
}

impl guest::Link for Link {
    fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal> {
        let conn = self
            .endpoint
            .open(self.peer, port)
            .map_err(refusal("opening a connection to the node over QUIC"))?;
        let socket = bridge::socket(conn)
            .map_err(|e| Refusal::os("bridging a QUIC connection to a local socket", e))?;
        guest::own_duplex(socket)
    }

    /// The open is queued without waiting for the node's answer, so the deadline bounds nothing more than it.
    fn dial_before(&self, port: u32, deadline: Option<Instant>) -> Result<DuplexOwner, Refusal> {
        if deadline.is_some_and(|at| at <= Instant::now()) {
            return Err(Refusal::Os {
                op: "opening a connection to the node over QUIC",
                why: "the caller's deadline passed".to_owned(),
            });
        }
        self.dial(port)
    }
}

/// The node agent's client over QUIC: the guest's protocol, its waits QUIC's own (`hex_client::guest::Quic`).
pub type Client = guest::Client<guest::Quic>;

/// A client of the node agent named `name` at `address`, over `endpoint`: dialed, said hello to, and held to the
/// node's own bound on connections, by `deadline` (none: the dial until the driver tells it, [`Endpoint::dial`], and
/// the hello on its kind's learned wait, `client.quic.answer.hello`).
pub fn connect(
    endpoint: Arc<Endpoint>,
    name: &str,
    address: SocketAddr,
    deadline: Option<Instant>,
) -> Result<(Client, PeerId), Refused> {
    let peer = endpoint.dial(name, address, deadline)?;
    let link = guest::own_link(Link {
        endpoint: Arc::clone(&endpoint),
        peer,
    })
    .map_err(Refused::Node)?;
    let client = Client::connect_timed_before(link, deadline).map_err(Refused::Node)?;
    let lanes = u32::try_from(client.limits().connections).unwrap_or(u32::MAX);
    endpoint.limit(peer, lanes)?;
    Ok((client, peer))
}
