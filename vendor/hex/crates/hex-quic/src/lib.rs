//! Hex's wire over hyper-transport on hyper-quic (docs/design/hex-quic.md): a node and its peers, kiln's balancer
//! among them, each holding a certificate kiln's CA issued from its own key's request ([`enroll`]), talk over one
//! QUIC connection a pair under TLS 1.3 with `TLS13_AES_256_GCM_SHA384` alone ([`tls`]). On it, each of a node's
//! ports (its control, a program's stdio streams, staged file trees, its log) is a byte stream of its own on
//! hyper-transport's lanes, as [`hex_core::lanes`] lays them out, which a peer opens ([`Endpoint::open`]) and the
//! node accepts ([`Endpoint::accept`]). [`link`] runs the node agent's client over it.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

pub mod bridge;
pub mod capacity;
pub mod classes;
mod conn;
mod driver;
pub mod enroll;
pub mod limits;
pub mod link;
pub mod tls;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::Instant;

pub use conn::{Conn, HELD_AT, Notify};
pub use driver::peer_id;
pub use hex_core::lanes::Why;
pub use hyper_quic::rustls::{CipherSuite, NamedGroup};
pub use hyper_transport::PeerId;
pub use limits::Shape;

use driver::{Accepts, Command, Commands, Driver};

/// Why the transport refused, typed (CLAUDE.md §3).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// Its configuration can't be used: what, and why.
    #[error("{what} can't be used: {why}")]
    Configuration { what: &'static str, why: String },
    #[error("{op} failed: {why}")]
    Os { op: &'static str, why: String },
    /// hyper-transport refused, as it says.
    #[error("the transport refused: {refusal}")]
    Transport { refusal: hyper_transport::Refusal },
    /// The peer's certificate isn't one kiln's CA issued, names another peer than the one dialed, or names a peer the
    /// endpoint it reached doesn't admit: refused at the handshake, on either side (docs/design/hex-quic.md Q5, Q10).
    #[error("the peer's identity isn't one kiln's CA enrolled for it, or one the endpoint admits")]
    Identity,
    /// Nothing answered: the path, a timeout or a reset.
    #[error("the peer didn't answer")]
    Unreachable,
    /// The connection to the peer closed, and why where either side said.
    #[error("the connection to the peer closed{}", .refusal.map(|r| format!(": {r}")).unwrap_or_default())]
    Closed {
        refusal: Option<hyper_transport::Refusal>,
    },
    #[error("no connection to the peer")]
    NotConnected,
    /// A connection to a port was refused or reset.
    #[error("the connection to the port was refused: {why}")]
    Conn { why: Why },
    /// The peer broke the lanes' protocol: its connection was closed.
    #[error("the peer broke the lanes' protocol, and its connection was closed")]
    Broken,
    /// The caller's deadline passed waiting for `what`.
    #[error("the caller's deadline passed waiting for {what}")]
    Deadline { what: &'static str },
    /// The CA refused the enrollment.
    #[error("kiln's CA refused: {0}")]
    Ca(enroll::CaRefused),
    /// The CA's answer isn't a certificate for the request.
    #[error("the CA's answer can't be taken: {why}")]
    Issued { why: String },
    /// The node agent refused, as it said.
    #[error("the node agent refused: {0}")]
    Node(hex_api::Refusal),
    /// The endpoint's driver has ended.
    #[error("the endpoint has ended")]
    Ended,
    /// A capacity message can't be carried: its bytes aren't one, or it passes the bound
    /// (docs/design/capacity-view.md CV9).
    #[error("a capacity message can't be carried: {0}")]
    Message(hex_core::wire::Fault),
}

impl Refused {
    pub(crate) fn closed(refusal: Option<hyper_transport::Refusal>) -> Self {
        match refusal {
            Some(hyper_transport::Refusal::Identity) => Refused::Identity,
            refusal => Refused::Closed { refusal },
        }
    }

    pub(crate) fn unreachable(refusal: Option<hyper_transport::Refusal>) -> Self {
        match refusal {
            Some(hyper_transport::Refusal::Identity) => Refused::Identity,
            Some(refusal) => Refused::Transport { refusal },
            None => Refused::Unreachable,
        }
    }
}

/// Which of the peers kiln's CA enrolled may open a connection to an endpoint (docs/design/hex-quic.md Q10). Any other
/// is refused at its handshake, as a peer kiln's CA didn't enroll is ([`Refused::Identity`] on its side), and is
/// charged to no identity, so it holds none of the endpoint's connections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admits {
    /// Every peer kiln's CA enrolled: a node's, whose peers (kiln's balancer, the fleet's nodes) no configuration lists.
    Enrolled,
    /// The peers enrolled as these names alone: a capacity lane's holders (docs/design/capacity-view.md CV8, CV9).
    Named(BTreeSet<String>),
}

impl Admits {
    /// Whether a connection opened by the peer whose certificate names `name` is admitted.
    pub(crate) fn admits(&self, name: &str) -> bool {
        match self {
            Admits::Enrolled => true,
            Admits::Named(names) => names.contains(name),
        }
    }
}

/// What an endpoint is made with.
pub struct Config {
    pub credentials: tls::Credentials,
    pub bind: SocketAddr,
    /// It accepts connections (a node), or only dials (a client).
    pub listen: bool,
    /// The ports it serves its peers.
    pub serves: Vec<u32>,
    pub shape: Shape,
}

/// An endpoint: its driver's thread, which ends as it drops, every connection on it told why.
pub struct Endpoint {
    commands: Commands,
    accepts: Arc<Accepts>,
    local: SocketAddr,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Endpoint {
    /// Binds `config.bind` and starts the driver, admitting every peer kiln's CA enrolled ([`Admits::Enrolled`]).
    pub fn start(config: Config) -> Result<Self, Refused> {
        Self::start_admitting(config, Admits::Enrolled)
    }

    /// Binds `config.bind` and starts the driver, admitting the connections of the peers `admits` names alone: the
    /// rule holds from the endpoint's first datagram.
    pub fn start_admitting(config: Config, admits: Admits) -> Result<Self, Refused> {
        let limits = limits::limits(config.shape);
        let derived = limits::derived(config.shape, &limits);
        let budget = limits::budget(&limits);
        let tls = tls::tls(&config.credentials, config.listen)?;
        let ep = hyper_transport::Config {
            tls,
            role: classes::Role::Member,
            limits,
            congestion: Default::default(),
        };
        let started =
            Driver::start_admitting(ep, budget, config.bind, derived, config.serves, admits)?;
        Ok(Self {
            commands: started.commands,
            accepts: started.accepts,
            local: started.local,
            thread: Some(started.thread),
        })
    }

    /// The address its socket is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Dials the peer named `name` at `address`, its certificate checked for that name, until `deadline` (none: until
    /// the driver tells it, as it tells every dial: a handshake nothing answers ends once QUIC's idle timeout passes
    /// with nothing from the peer, RFC 9000 §10.1, Q8; a connected peer's Welcome is the peer's to send): the peer's
    /// id once the connection is up, or why it isn't.
    pub fn dial(
        &self,
        name: &str,
        address: SocketAddr,
        deadline: Option<Instant>,
    ) -> Result<PeerId, Refused> {
        let (reply, answer) = hex_wait::mpsc::sync_channel(1).map_err(|why| Refused::Os {
            op: "making a dial's answer",
            why: why.to_string(),
        })?;
        self.commands.send(Command::Dial {
            name: name.to_owned(),
            address,
            reply,
        })?;
        driver::wait_dial(&answer, deadline)
    }

    /// Opens a connection to `peer`'s `port`: bytes may be written at once; a refusal comes as its first read's
    /// error.
    pub fn open(&self, peer: PeerId, port: u32) -> Result<Conn, Refused> {
        let (reply, answer) = sync_channel(1);
        self.commands.send(Command::Open { peer, port, reply })?;
        let shared = answer.recv().map_err(|_| Refused::Ended)??;
        Ok(driver::conn(shared))
    }

    /// The next connection a peer opened to `port`, waiting for one until `deadline` (none: until one comes).
    pub fn accept(&self, port: u32, deadline: Option<Instant>) -> Result<(PeerId, Conn), Refused> {
        let (peer, shared) = self.accepts.take(port, deadline)?;
        Ok((peer, driver::conn(shared)))
    }

    /// What takes `port`'s connections apart from the endpoint, for a thread of their own: it ends, refused
    /// [`Refused::Ended`], once the endpoint has.
    pub fn acceptor(&self, port: u32) -> Acceptor {
        Acceptor {
            accepts: Arc::clone(&self.accepts),
            port,
        }
    }

    /// The suite and group the connection to `peer` negotiated.
    pub fn negotiated(
        &self,
        peer: PeerId,
    ) -> Result<Option<(CipherSuite, Option<NamedGroup>)>, Refused> {
        let (reply, answer) = sync_channel(1);
        self.commands.send(Command::Negotiated { peer, reply })?;
        answer.recv().map_err(|_| Refused::Ended)
    }

    /// `peer` holds at most `lanes` connections at once (its hello's bound): this side opens no more.
    pub fn limit(&self, peer: PeerId, lanes: u32) -> Result<(), Refused> {
        self.commands.send(Command::Limit { peer, lanes })
    }
}

/// One port's accepted connections, taken apart from the endpoint ([`Endpoint::acceptor`]).
pub struct Acceptor {
    accepts: Arc<Accepts>,
    port: u32,
}

impl Acceptor {
    /// The next connection a peer opened to the port, waiting until `deadline` (none: until one comes or the
    /// endpoint ends).
    pub fn accept(&self, deadline: Option<Instant>) -> Result<(PeerId, Conn), Refused> {
        let (peer, shared) = self.accepts.take(self.port, deadline)?;
        Ok((peer, driver::conn(shared)))
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests;
