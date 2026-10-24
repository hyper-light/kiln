//! What a round of the plane costs a message in time: `cargo bench -p hyper-datagram --bench round`
//! (docs/benchmarks.md, "hyper-datagram against slates' seal").
//!
//! The round is the allocation bench's (`benches/allocs.rs`): one plane holds `P` peers, queues for
//! each as many messages of one size as a 1,232-byte path packs, flushes one sealed datagram a peer,
//! and each datagram is opened at its peer's plane. A cell times 32,000 datagrams after 100 warm
//! rounds, in one thread: the plane's own cost, a peer found for every message queued and every
//! datagram opened among it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_macros,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    missing_docs
)]

use std::time::Instant;

use hyper_datagram::{
    AdmitAll, ExporterSecret, MAX_DATAGRAM_BYTES, OVERHEAD_BYTES, PeerId, Plane, PlaneLimits, Role,
    SECRET_BYTES,
};

/// The path size every peer is given, the allocation bench's: the IPv6 minimum MTU less the IP and
/// UDP headers (RFC 8200 §5: 1280; 40 + 8 bytes of headers), what any path carries.
const PATH: usize = 1_280 - 48;
/// Shape: the datagrams a cell times, as many for one peer as for sixteen (32,000 rounds of one,
/// 2,000 of sixteen): 14 to 26 ms a cell on an M5 Max, over ten thousand times the resolution of
/// the host's clock (a microsecond on macOS).
const DATAGRAMS: usize = 32_000;
/// Rounds before the clock starts, the allocation bench's.
const WARM: usize = 100;
/// The replay window's limit, the allocation bench's.
const WINDOW_LIMIT: usize = 1_024;

#[allow(
    clippy::disallowed_methods,
    reason = "a benchmark times itself on the host clock"
)]
fn host_now() -> Instant {
    Instant::now()
}

fn secret(peer: PeerId) -> ExporterSecret {
    let mut bytes = [0u8; SECRET_BYTES];
    bytes[0] = peer as u8;
    ExporterSecret::new(bytes)
}

/// The sending plane (id 0) and `peers` receiving planes, each with one epoch to the other.
fn planes(peers: usize) -> (Plane, Vec<Plane>) {
    let limits = PlaneLimits {
        max_peers: peers,
        epochs_per_peer: 2,
        window_limit: WINDOW_LIMIT,
    };
    let mut sender = Plane::new(0, limits).unwrap();
    let receivers = (1..=peers as PeerId)
        .map(|peer| {
            sender
                .install_epoch(peer, 1, &secret(peer), Role::Initiator)
                .unwrap();
            sender.set_path(peer, PATH).unwrap();
            let mut receiver = Plane::new(
                peer,
                PlaneLimits {
                    max_peers: 1,
                    ..limits
                },
            )
            .unwrap();
            receiver
                .install_epoch(0, 1, &secret(peer), Role::Acceptor)
                .unwrap();
            receiver
        })
        .collect();
    (sender, receivers)
}

/// Where flushed datagrams land: one fixed buffer per peer, as a socket's send would read them.
struct Wire {
    buffers: Vec<Vec<u8>>,
    lengths: Vec<usize>,
}

fn round(
    sender: &mut Plane,
    receivers: &mut [Plane],
    message: &[u8],
    messages: usize,
    wire: &mut Wire,
) {
    for peer in 1..=receivers.len() as PeerId {
        for _ in 0..messages {
            sender.queue(peer, message).unwrap();
        }
    }
    sender.flush(|peer, datagram| {
        let datagram = datagram.unwrap();
        let slot = peer as usize - 1;
        wire.buffers[slot][..datagram.len()].copy_from_slice(datagram);
        wire.lengths[slot] = datagram.len();
    });
    for (slot, receiver) in receivers.iter_mut().enumerate() {
        let datagram = &mut wire.buffers[slot][..wire.lengths[slot]];
        let opened = receiver.open(datagram, &AdmitAll).unwrap();
        assert_eq!(opened.messages().count(), messages);
    }
}

/// Nanoseconds a message of `peers` peers' rounds of messages of `size` bytes.
fn cell(peers: usize, size: usize) -> (usize, f64) {
    // As many messages as fit the path: what a busy consensus round packs.
    let messages = (PATH - OVERHEAD_BYTES) / (size + 2);
    let message = vec![0x5a; size];
    let (mut sender, mut receivers) = planes(peers);
    let mut wire = Wire {
        buffers: (0..peers).map(|_| vec![0; MAX_DATAGRAM_BYTES]).collect(),
        lengths: vec![0; peers],
    };
    for _ in 0..WARM {
        round(&mut sender, &mut receivers, &message, messages, &mut wire);
    }
    let rounds = DATAGRAMS / peers;
    let began = host_now();
    for _ in 0..rounds {
        round(&mut sender, &mut receivers, &message, messages, &mut wire);
    }
    let ns = began.elapsed().as_nanos() as f64 / (rounds * peers * messages) as f64;
    (messages, ns)
}

fn main() {
    println!(
        "hyper-datagram: ns a message of a round, {DATAGRAMS} datagrams a cell after {WARM} rounds"
    );
    println!(
        "  {:>5} {:>7} {:>8} {:>10}",
        "peers", "message", "messages", "ns"
    );
    for peers in [1usize, 16] {
        for size in [16usize, 128, 1_024] {
            let (messages, ns) = cell(peers, size);
            println!("  {peers:>5} {size:>7} {messages:>8} {ns:>10.2}");
        }
    }
}
