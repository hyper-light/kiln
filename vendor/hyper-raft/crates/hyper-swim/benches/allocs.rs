//! What a protocol period costs the allocator and the page tables: `cargo bench -p hyper-swim
//! --bench allocs` (docs/benchmarks.md, "hyper-swim").
//!
//! `N` detectors in one process run whole periods as a member's driver does (`tests/cluster.rs`):
//! each ticks, sends its probe target a ping carrying gossip, the target applies it and answers
//! with an acknowledgement carrying its own gossip and coordinate, and the prober applies that,
//! credits the probe and folds the round trip into its coordinate. Every message goes through the
//! wire codec. The count is per member per period, after the cluster has converged: quiet (no
//! membership changes) and churning (one member refutes a suspicion every period, so its new
//! incarnation spreads through the gossip).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_macros,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    clippy::unreachable,
    missing_docs
)]

use std::num::NonZeroUsize;
use std::time::Duration;

use hyper_datagram::{LENGTH_BYTES, OVERHEAD_BYTES};
use hyper_measure::{alloc, faults};
use hyper_swim::HostId;
use hyper_swim::codec::{Coordinate, GossipBatch, SwimMessage, gossip_capacity};
use hyper_swim::coordinates::NetworkCoordinate;
use hyper_swim::detector::{Detector, OUTSTANDING, Ping, PingReq};
use hyper_swim::membership::{Liveness, MemberState};
use hyper_timing::Exposure;

#[global_allocator]
static ALLOCATOR: alloc::Counting = alloc::Counting;

/// Periods counted per row, after the warm-up.
const PERIODS: u64 = 400;
/// The path's datagram size: QUIC's minimum (RFC 9000 §14.1), as `tests/cluster.rs` sets it.
const DATAGRAM: usize = 1_200;
/// The workload's round trip: 200 µs, a LAN's.
const RTT_NS: u64 = 200_000;
/// The workload's jitter: up to half the round trip more, from a xorshift stream.
const JITTER_NS: u64 = RTT_NS / 2;
/// How late each wake comes: Linux's default timer slack, 50 µs (`PR_SET_TIMERSLACK(2const)`).
const LATE_NS: u64 = 50_000;

/// One member: its detector, its own clock, and the acknowledgements in flight to it: as many as
/// its detector keeps probe records for (`OUTSTANDING`), an older answer being one no record
/// credits.
struct Member {
    detector: Detector,
    now: u64,
    flights: [Flight; OUTSTANDING],
}

/// An acknowledgement in flight: its encoding, and when it lands. The buffer grows once to the
/// largest acknowledgement.
#[derive(Default)]
struct Flight {
    landing: Option<u64>,
    bytes: Vec<u8>,
}

impl Member {
    /// When the next acknowledgement in flight lands.
    fn landing(&self) -> Option<u64> {
        self.flights
            .iter()
            .filter_map(|flight| flight.landing)
            .min()
    }

    /// The flight a new acknowledgement takes: a free one, or else the one that lands first, whose
    /// answer is then lost.
    fn flight(&mut self) -> &mut Flight {
        let at = self
            .flights
            .iter()
            .position(|flight| flight.landing.is_none())
            .or_else(|| (0..self.flights.len()).min_by_key(|at| self.flights[*at].landing))
            .unwrap();
        &mut self.flights[at]
    }

    /// Applies, in the order they land, the acknowledgements in flight that have landed by this
    /// member's clock: their gossip, the answerer's coordinate, and the answer at its landing.
    fn land(&mut self) {
        while let Some((at, landing)) = self
            .flights
            .iter()
            .enumerate()
            .filter_map(|(at, flight)| flight.landing.map(|landing| (at, landing)))
            .filter(|(_, landing)| *landing <= self.now)
            .min_by_key(|(_, landing)| *landing)
        {
            self.flights[at].landing = None;
            let SwimMessage::Ack {
                from,
                gossip,
                coordinate,
                nonce,
                relay_within_ns,
                ..
            } = SwimMessage::decode(&self.flights[at].bytes).unwrap()
            else {
                unreachable!()
            };
            self.detector.apply_gossip(gossip);
            self.detector.learn_coordinate(from, coordinate);
            self.detector.on_ack(from, nonce, landing, relay_within_ns);
        }
    }
}

/// Gossip entries a message carries: what the datagram holds beside an acknowledgement, as
/// `tests/cluster.rs` derives it.
fn gossip_per_message() -> usize {
    let mut bare = Vec::new();
    SwimMessage::Ack {
        from: HostId(0),
        nonce: u64::MAX,
        boot_nonce: 1,
        configuration_version: 1,
        standing: None,
        promise_ns: 0,
        relay_within_ns: u64::MAX,
        gossip: GossipBatch::Entries(&[]),
        coordinate: Coordinate::Held(&NetworkCoordinate::origin()),
    }
    .encode_into(&mut bare);
    gossip_capacity(DATAGRAM - OVERHEAD_BYTES - LENGTH_BYTES, bare.len())
}

/// What a member's driver holds across periods: the gossip batch it fills, the bytes it encodes,
/// the relays it is asked for, and the workload's noise.
struct Buffers {
    gossip: usize,
    batch: Vec<(HostId, MemberState)>,
    ping: Vec<u8>,
    /// Members a chunk of a view carries, and the chunk's bytes.
    view_room: usize,
    view: Vec<u8>,
    requests: Vec<PingReq>,
    noise: u64,
}

impl Buffers {
    fn new() -> Self {
        Self {
            gossip: gossip_per_message(),
            batch: Vec::new(),
            ping: Vec::new(),
            view_room: view_room(),
            view: Vec::new(),
            requests: Vec::new(),
            noise: 0x2545_F491_4F6C_DD1D,
        }
    }

    /// A xorshift step (Marsaglia 2003).
    fn round_trip(&mut self) -> u64 {
        self.noise ^= self.noise << 13;
        self.noise ^= self.noise >> 7;
        self.noise ^= self.noise << 17;
        RTT_NS + self.noise % JITTER_NS
    }
}

fn cluster(members: usize) -> Vec<Member> {
    (0..members as u64)
        .map(|id| {
            let mut detector = Detector::new(
                HostId(id),
                Exposure::new(),
                NonZeroUsize::new(members).unwrap(),
                // The simulation's stamps are whole nanoseconds.
                Duration::from_nanos(1),
                // No detection budget: periods at what their probes need, as the counts in docs/benchmarks.md were taken.
                Duration::ZERO,
            );
            for peer in 0..members as u64 {
                detector.join(HostId(peer)).unwrap();
            }
            Member {
                detector,
                now: 1,
                flights: Default::default(),
            }
        })
        .collect()
}

/// Advances member `prober` to its next period and runs the probe it starts: the ping through
/// the codec to its target, which applies its gossip and answers, and the acknowledgement back.
fn step(members: &mut [Member], prober: usize, buffers: &mut Buffers) {
    let ping = loop {
        let member = &mut members[prober];
        // Its wake, late by the timer's slack, or the next answer's landing, whichever comes first.
        let at = match (member.detector.wake(), member.landing()) {
            (Some(wake), Some(landing)) => (wake + LATE_NS).min(landing),
            (Some(wake), None) => wake + LATE_NS,
            (None, Some(landing)) => landing,
            (None, None) => member.now,
        };
        member.now = member.now.max(at);
        member.land();
        if let Some(ping) = member.detector.poll(member.now, &mut buffers.requests) {
            break ping;
        }
    };
    exchange(members, prober, ping, buffers);
    sync(members, prober, buffers);
}

/// Members a chunk of a view carries: what the datagram holds beside an empty one, as
/// `tests/cluster.rs` derives it.
fn view_room() -> usize {
    let mut bare = Vec::new();
    SwimMessage::Sync {
        from: HostId(0),
        boot_nonce: 1,
        digest: 0,
        pull: true,
        gossip: GossipBatch::Entries(&[]),
    }
    .encode_into(&mut bare);
    gossip_capacity(DATAGRAM - OVERHEAD_BYTES - LENGTH_BYTES, bare.len())
}

/// Delivers, through the codec, every chunk of member `sender`'s view its anti-entropy exchanges
/// ask now: the answer it owes a pull and the exchange it began, as a member's driver does after
/// each poll. A pull it carries is answered at the receiver's next step.
fn sync(members: &mut [Member], sender: usize, buffers: &mut Buffers) {
    while let Some(chunk) = members[sender]
        .detector
        .sync_into(buffers.view_room, &mut buffers.batch)
    {
        SwimMessage::Sync {
            from: HostId(sender as u64),
            boot_nonce: 1,
            digest: chunk.digest,
            pull: chunk.pull,
            gossip: GossipBatch::Entries(&buffers.batch),
        }
        .encode_into(&mut buffers.view);
        let SwimMessage::Sync {
            from,
            digest,
            pull,
            gossip,
            ..
        } = SwimMessage::decode(&buffers.view).unwrap()
        else {
            unreachable!()
        };
        members[chunk.to.0 as usize]
            .detector
            .on_sync(from, digest, pull, gossip);
    }
}

fn exchange(members: &mut [Member], prober: usize, ping: Ping, buffers: &mut Buffers) {
    let target = ping.to.0 as usize;
    let landing = members[prober].now + buffers.round_trip();
    members[prober]
        .detector
        .ping_gossip_into(ping.to, buffers.gossip, &mut buffers.batch);
    SwimMessage::Ping {
        from: HostId(prober as u64),
        nonce: ping.nonce,
        boot_nonce: 1,
        configuration_version: 1,
        gossip: GossipBatch::Entries(&buffers.batch),
    }
    .encode_into(&mut buffers.ping);
    let SwimMessage::Ping {
        from,
        gossip,
        nonce,
        ..
    } = SwimMessage::decode(&buffers.ping).unwrap()
    else {
        unreachable!()
    };
    let answering = &mut members[target].detector;
    answering.apply_gossip(gossip);
    let ack = answering.on_ping(from);
    answering.ack_gossip_into(from, buffers.gossip, &mut buffers.batch);
    let coordinate = *answering.coordinate();
    // In flight to the prober, which applies it as it lands, a round trip after the ping.
    let flight = members[prober].flight();
    SwimMessage::Ack {
        from: ping.to,
        nonce,
        boot_nonce: 1,
        configuration_version: 1,
        standing: None,
        promise_ns: 0,
        relay_within_ns: ack.relay_within_ns,
        gossip: GossipBatch::Entries(&buffers.batch),
        coordinate: Coordinate::Held(&coordinate),
    }
    .encode_into(&mut flight.bytes);
    flight.landing = Some(landing);
}

/// One period of every member.
fn period(members: &mut [Member], buffers: &mut Buffers) {
    for prober in 0..members.len() {
        step(members, prober, buffers);
    }
}

/// Runs whole rounds until every member's every pair is configured by its own estimator: the
/// fact the counted periods start from.
fn warm(members: &mut [Member], buffers: &mut Buffers) {
    let n = members.len() as u64;
    let configured = |members: &[Member]| {
        members.iter().enumerate().all(|(id, member)| {
            (0..n).filter(|peer| *peer != id as u64).all(|peer| {
                member
                    .detector
                    .report(HostId(peer))
                    .is_some_and(|report| report.configured)
            })
        })
    };
    while !configured(members) {
        for _ in 0..n {
            period(members, buffers);
        }
    }
    // A further round for every report the configuration's gossip queued to drain.
    for _ in 0..n {
        period(members, buffers);
    }
}

/// One member hears itself suspected and refutes, so a new incarnation spreads.
fn churn(members: &mut [Member], at: u64) {
    let member = (at as usize) % members.len();
    let detector = &mut members[member].detector;
    let incarnation = detector.membership().local_incarnation();
    detector
        .apply(
            HostId(member as u64),
            MemberState {
                liveness: Liveness::Suspect,
                incarnation,
            },
        )
        .unwrap();
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

fn row(what: &str, members: usize, cost: &Cost) {
    println!(
        "  {what:<12} {members:>8} {:>10.2} {:>10.2} {:>10.1} {:>10.4}",
        cost.allocations, cost.reallocations, cost.bytes, cost.faults
    );
}

fn point(members: usize) {
    let mut cluster = cluster(members);
    let mut buffers = Buffers::new();
    warm(&mut cluster, &mut buffers);
    let n = PERIODS * members as u64;
    let cost = counted(n, || {
        for _ in 0..PERIODS {
            period(&mut cluster, &mut buffers);
        }
    });
    row("quiet", members, &cost);
    let mut at = 0;
    let cost = counted(n, || {
        for _ in 0..PERIODS {
            at += 1;
            churn(&mut cluster, at);
            period(&mut cluster, &mut buffers);
        }
    });
    row("churning", members, &cost);
    for member in &cluster {
        assert_eq!(
            member.detector.membership().alive().count(),
            members,
            "every member stays alive"
        );
    }
}

fn main() {
    assert!(alloc::installed(), "the counting allocator is installed");
    println!(
        "hyper-swim: allocations, reallocations, bytes asked and minor faults per member per \
         period ({PERIODS} periods, after every pair is configured); {} gossip entries a message",
        gossip_per_message()
    );
    println!(
        "  {:<12} {:>8} {:>10} {:>10} {:>10} {:>10}",
        "", "members", "allocs", "reallocs", "bytes", "faults"
    );
    for members in [4usize, 16, 64, 256] {
        point(members);
    }
}
