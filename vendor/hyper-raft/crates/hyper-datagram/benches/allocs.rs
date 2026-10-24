//! What sealing and opening cost the allocator and the page tables: `cargo bench -p
//! hyper-datagram --bench allocs` (docs/benchmarks.md, "Allocations").
//!
//! Two planes hold `P` peers each. One round queues `M` messages of one size for every peer,
//! flushes (one sealed datagram a peer) and opens every datagram at the other plane. The messages
//! and the buffers the datagrams are copied into are built before the count begins, so the count
//! is the plane's own: the queueing, the seal, the open and the replay window. The rows after the
//! steady state count what the plane does once: the window widening to a reordered datagram.
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

use hyper_datagram::{
    AdmitAll, ExporterSecret, MAX_DATAGRAM_BYTES, PeerId, Plane, PlaneLimits, Role, SECRET_BYTES,
};
use hyper_measure::{alloc, faults};

#[global_allocator]
static ALLOCATOR: alloc::Counting = alloc::Counting;

/// Rounds counted per row, after the warm-up rounds.
const ROUNDS: u64 = 2_000;
const WARM: u64 = 100;
/// The path size every peer is given: the IPv6 minimum MTU less the IP and UDP headers
/// (RFC 8200 §5: 1280; 40 + 8 bytes of headers), what any path carries.
const PATH: usize = 1_280 - 48;
const WINDOW_LIMIT: usize = 1_024;

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

impl Wire {
    fn new(peers: usize) -> Self {
        Self {
            buffers: (0..peers).map(|_| vec![0; MAX_DATAGRAM_BYTES]).collect(),
            lengths: vec![0; peers],
        }
    }
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

struct Cost {
    allocations: f64,
    reallocations: f64,
    bytes: f64,
    faults: f64,
}

fn counted(n: u64, work: impl FnOnce()) -> Cost {
    let before = faults::read().unwrap();
    alloc::begin();
    work();
    let counts = alloc::end();
    let after = faults::read().unwrap();
    let n = n as f64;
    Cost {
        allocations: counts.allocations as f64 / n,
        reallocations: counts.reallocations as f64 / n,
        bytes: counts.bytes as f64 / n,
        faults: after.since(&before).minor as f64 / n,
    }
}

fn row(what: &str, peers: usize, size: usize, messages: usize, cost: &Cost) {
    println!(
        "  {what:<34} {peers:>5} {size:>7} {messages:>8} {:>10.3} {:>10.3} {:>10.1} {:>10.4}",
        cost.allocations, cost.reallocations, cost.bytes, cost.faults
    );
}

fn steady(peers: usize, size: usize) {
    // As many messages as fit the path: what a busy consensus round packs.
    let messages = (PATH - hyper_datagram::OVERHEAD_BYTES) / (size + 2);
    let message = vec![0x5a; size];
    let (mut sender, mut receivers) = planes(peers);
    let mut wire = Wire::new(peers);
    for _ in 0..WARM {
        round(&mut sender, &mut receivers, &message, messages, &mut wire);
    }
    let cost = counted(ROUNDS * peers as u64, || {
        for _ in 0..ROUNDS {
            round(&mut sender, &mut receivers, &message, messages, &mut wire);
        }
    });
    row("queue, seal and open", peers, size, messages, &cost);
}

/// One datagram held back while later ones open, then opened: the window widens once.
fn reordered() {
    let (mut sender, mut receivers) = planes(1);
    let mut held = Vec::new();
    sender.queue(1, b"late").unwrap();
    sender.flush(|_, datagram| held = datagram.unwrap().to_vec());
    let mut wire = Wire::new(1);
    for _ in 0..40 {
        round(&mut sender, &mut receivers, b"on time", 1, &mut wire);
    }
    let cost = counted(1, || {
        receivers[0].open(&mut held, &AdmitAll).unwrap();
    });
    row("open 40 late, widening the window", 1, 4, 1, &cost);
}

fn main() {
    assert!(alloc::installed(), "the counting allocator is installed");
    println!(
        "hyper-datagram: allocations, reallocations, bytes asked and minor faults per datagram, \
         the calling thread's ({ROUNDS} rounds after {WARM}, a {PATH}-byte path)"
    );
    println!(
        "  {:<34} {:>5} {:>7} {:>8} {:>10} {:>10} {:>10} {:>10}",
        "", "peers", "message", "messages", "allocs", "reallocs", "bytes", "faults"
    );
    for peers in [1usize, 16] {
        for size in [16usize, 128, 1_024] {
            steady(peers, size);
        }
    }
    reordered();
}
