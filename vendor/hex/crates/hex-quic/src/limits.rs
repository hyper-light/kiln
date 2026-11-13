//! The endpoint's numbers (docs/design/hex-quic.md Q8): each derived where the endpoint is made, from the RFCs that
//! fix QUIC's own, from hyper-transport's derived bounds, and from the owner's own resource bounds. None is picked.

use std::time::Duration;

use hyper_transport::{
    Acknowledgement, AdmissionLimits, AskQueue, Fixed, Limits, RECEIVE_CHUNK, StreamCredit,
    connection_charge, frame_grant, stream_window_ceiling,
};

use crate::classes::{self, Hex};

/// RFC 9002 §6.2.2: the initial RTT before a path has a sample, `kInitialRtt`, 333 milliseconds.
const INITIAL_RTT: Duration = Duration::from_millis(333);
/// RFC 9000 §18.2: `max_ack_delay`'s default where a peer states none, 25 milliseconds.
const MAX_ACK_DELAY: Duration = Duration::from_millis(25);

/// RFC 9002 §6.2.1's probe timeout before a path has a sample: `smoothed_rtt + max(4 * rttvar, kGranularity) +
/// max_ack_delay`, with `smoothed_rtt` the initial RTT and `rttvar` half of it (§5.3): 1,024 milliseconds. QUIC
/// raises what is derived from it with the path's own PTO once it has one (RFC 9000 §10.1).
pub fn initial_pto() -> Duration {
    let rttvar = INITIAL_RTT.checked_div(2).unwrap_or(INITIAL_RTT);
    INITIAL_RTT
        .saturating_add(rttvar.saturating_mul(4).max(hyper_transport::K_GRANULARITY))
        .saturating_add(MAX_ACK_DELAY)
}

/// What an endpoint holds, its owner's own bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    /// Connections to peers it holds in all: a node's connections bound (each holds one of its descriptors), a
    /// client's one node.
    pub connections: usize,
    /// Connections to ports one peer holds with it at once, each on a lane each way.
    pub lanes: u32,
}

/// The driver's own counts, from the shape and the limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Derived {
    pub(crate) lanes: u32,
    /// Commands the queue holds before a sender waits: each of a peer's connections has at most two outstanding
    /// (a frame placed or an End or a Reset, and its owner's word that it read all).
    pub(crate) commands: usize,
    /// Datagrams one turn reads before it runs what was asked: the chunks the endpoint holds them in.
    pub(crate) receive_turn: usize,
}

/// hyper-transport's limits for `shape`.
pub(crate) fn limits(shape: Shape) -> Limits {
    let pto = initial_pto();
    let window = stream_window_ceiling();
    let chunk = u64::try_from(RECEIVE_CHUNK).unwrap_or(u64::MAX).max(1);
    let chunks_a_window = usize::try_from(window.div_ceil(chunk)).unwrap_or(usize::MAX);
    Limits {
        admission: AdmissionLimits {
            pending: shape.connections,
            identities: shape.connections,
            connections: shape.connections,
            // A peer's connection and the one replacing it after its path changed.
            per_identity: shape.connections.min(2),
        },
        // The least the endpoint accepts: Hex opens no exchange.
        streams_per_connection: 1,
        stream_credit: StreamCredit::Fixed,
        ask_queue: AskQueue::Table,
        exchanges: 1,
        // Every connection's lane, and lane 0, the refusals'.
        lanes_per_peer: shape.lanes.saturating_add(1),
        // A lossless lane holds one frame past what QUIC took (hyper-transport `send_frame`), and the driver
        // polls each frame as it comes.
        lane_window: 1,
        // No exchange: a peer's head is refused.
        max_head: 0,
        window_ceiling: window,
        serve_period: pto,
        // RFC 9000 §10.1's floor, three probe timeouts.
        idle_timeout: pto.saturating_mul(3),
        // Three fit in the idle timeout: a connection idle between calls stays up.
        keep_alive: Some(pto),
        // The driver polls transmits after each datagram it hands the endpoint.
        max_responses: 1,
        receive_chunks: shape.connections.saturating_mul(chunks_a_window),
        retry_bytes: None,
        // Hex's control plane asks and answers: an acknowledgement rides the answer where it goes sooner.
        acknowledgement: Acknowledgement::Replies,
        fec_window: None,
    }
}

/// The driver's counts for `shape` under `limits`.
pub(crate) fn derived(shape: Shape, limits: &Limits) -> Derived {
    Derived {
        lanes: shape.lanes,
        commands: usize::try_from(shape.lanes)
            .unwrap_or(usize::MAX)
            .saturating_mul(2)
            .max(1),
        receive_turn: limits.receive_chunks.max(1),
    }
}

/// The bytes the endpoint may hold, summed from what its limits let it hold: every connection's and handshake's
/// window at its ceiling and its state's charge; every lane's one frame; and its receive chunks. The spare buffers
/// it keeps are as many as the frames it can hold at once.
pub(crate) fn budget(limits: &Limits) -> Fixed {
    let held = limits
        .admission
        .connections
        .saturating_add(limits.admission.pending);
    let held = u64::try_from(held).unwrap_or(u64::MAX);
    let per_connection = limits
        .window_ceiling
        .saturating_add(connection_charge::<Hex>(limits));
    let frames = u64::try_from(limits.admission.connections)
        .unwrap_or(u64::MAX)
        .saturating_mul(u64::from(limits.lanes_per_peer))
        .saturating_mul(u64::try_from(limits.lane_window).unwrap_or(u64::MAX));
    let chunks = u64::try_from(limits.receive_chunks)
        .unwrap_or(u64::MAX)
        .saturating_mul(u64::try_from(RECEIVE_CHUNK).unwrap_or(u64::MAX));
    let capacity = held
        .saturating_mul(per_connection)
        .saturating_add(frames.saturating_mul(frame_grant(classes::frame_bound())))
        .saturating_add(chunks);
    Fixed::new(capacity, usize::try_from(frames).unwrap_or(usize::MAX))
}

/// A connection's lanes on a client: what the process can bridge at once (docs/design/hex-quic.md Q8). On Unix each
/// bridged connection is a socket pair of two descriptors, so half the soft `RLIMIT_NOFILE` (getrlimit(2)). On Windows
/// each is a loopback TCP pair that holds two ports of the dynamic range (its listener's, which its accepted end keeps,
/// and its connecting end's): half of the range's 16,384 ports, IANA's 49152-65535 (RFC 6335 §6), Windows' own since
/// Vista (Microsoft KB 929851), not its handle table's 2^24, which no pair reaches. hyper-quic lays out a row for each
/// lane a peer may open when a connection starts (quinn's `StreamsState::new`), so this is paid per connection.
pub fn client_lanes() -> u32 {
    #[cfg(unix)]
    let pairs = rustix::process::getrlimit(rustix::process::Resource::Nofile)
        .current
        .unwrap_or(u64::MAX)
        / 2;
    #[cfg(windows)]
    let pairs: u64 = {
        /// RFC 6335 §6's dynamic ports, 49152 through 65535.
        const DYNAMIC_PORTS: u64 = 16_384;
        DYNAMIC_PORTS / 2
    };
    u32::try_from(pairs).unwrap_or(u32::MAX)
}

/// `wait` as poll(2) takes it, at most what a C `int` of milliseconds holds (poll's timeout where it has no `ppoll`,
/// and Winsock's `WSAPoll`): a longer wait waits this long at a time.
pub(crate) fn timespec(wait: Duration) -> rustix::event::Timespec {
    const MOST: Duration = Duration::from_millis(2_147_483_647);
    rustix::event::Timespec::try_from(wait.min(MOST)).unwrap_or(rustix::event::Timespec {
        tv_sec: 2_147_483,
        tv_nsec: 647_000_000,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HEX-QUIC-12 (docs/design/hex-quic.md Q8): the initial probe timeout is RFC 9002's 1,024 ms; the idle
    /// timeout is three of them and the keep-alive one; and the limits hyper-transport is given are ones it takes.
    #[test]
    fn hex_quic_12_the_numbers_are_the_rfcs_and_hyper_transport_takes_them() {
        assert_eq!(initial_pto(), Duration::from_millis(1_024));
        let node = limits(Shape {
            connections: 64,
            lanes: 192,
        });
        assert_eq!(node.idle_timeout, Duration::from_millis(3_072));
        assert_eq!(node.keep_alive, Some(Duration::from_millis(1_024)));
        assert_eq!(node.lanes_per_peer, 193);
        let client = limits(Shape {
            connections: 1,
            lanes: client_lanes(),
        });
        assert_eq!(client.admission.per_identity, 1);
        for limits in [node, client] {
            let budget = budget(&limits);
            let config = hyper_transport::Config {
                tls: tls_for_test(),
                role: crate::classes::Role::Member,
                limits,
                congestion: Default::default(),
            };
            let endpoint = hyper_transport::Endpoint::<Hex, Fixed, NoPeers>::new(
                config,
                budget,
                NoPeers,
                std::time::Instant::now(),
            );
            assert!(endpoint.is_ok(), "{:?}", endpoint.err());
        }
    }

    struct NoPeers;

    impl hyper_transport::Directory for NoPeers {
        type Role = crate::classes::Role;
        fn identify(
            &mut self,
            _: &hyper_transport::Authenticated<'_>,
        ) -> Option<(hyper_transport::PeerId, Self::Role)> {
            None
        }
        fn server_name(&self, _: hyper_transport::PeerId) -> Option<&str> {
            None
        }
    }

    fn tls_for_test() -> hyper_transport::tls::Tls {
        let ca = crate::enroll::sim::SimCa::new().unwrap();
        ca.admit("node.test", b"g");
        let node = crate::enroll::enroll(&ca, "node.test", b"g".to_vec()).unwrap();
        crate::tls::tls(&node, true).unwrap()
    }
}
