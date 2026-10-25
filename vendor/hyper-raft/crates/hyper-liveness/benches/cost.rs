//! What liveness costs a node, per second, for `G` groups over `P` peers: the node-pair stream
//! against what each project runs today. `cargo bench -p hyper-liveness --bench cost`
//! (docs/benchmarks.md, "hyper-liveness").
//!
//! - **The node-pair stream** (this crate): `P + 1` nodes, every pair sharing groups, on the
//!   simulated clock of `world.rs`; messages and the time spent in the crate's calls, per heartbeat
//!   sent or taken, measured.
//! - **Per-group heartbeats** (mantle's and focal's today): the Raft core they run (this
//!   repository's `hyper-raft`, which mantle vendors and into which focal's core changes are
//!   ported), with focal's `election_tick` 10 and `heartbeat_tick` 2 (`focal-consensus`'s defaults;
//!   mantle's range settings have the same shape). `G` groups a node of three voters each, placed
//!   round robin over the `P + 1` nodes, every group with a leader and nothing to replicate; every
//!   member ticks, every leader heartbeats its followers and they answer. Messages and the time of
//!   the ticks, the readies and the deliveries, per tick, measured.
//! - **slates' detector** (`hyper-swim`, slates' SWIM conformed): `P + 1` members, each probing one
//!   peer a period; messages and the time a period, measured.
//!
//! Per second, each is put at the same interval `η` between heartbeats on a pair (a group's
//! `heartbeat_tick` ticks; a SWIM member's round of `P` periods), so each detects as fast: the
//! interval the configurator chose on the macOS trace at 100 µs, 50 ms (`docs/timing.md` §2.6).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_macros,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    clippy::cognitive_complexity,
    missing_docs
)]

use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use hyper_raft::proto::{ConfState, Entry, HardState, Message, Snapshot};
use hyper_raft::{InitialState, RawNode, Storage, StorageError};
use hyper_swim::HostId;
use hyper_swim::detector::{Detector, Judge, OUTSTANDING, PingReq};
use hyper_timing::Exposure;

#[path = "support/world.rs"]
mod world;
use world::World;

/// The interval every design is put at: 50 ms, the macOS trace's configured `η` (`docs/timing.md`
/// §2.6, "The configurator on the measured inputs").
const ETA: Duration = Duration::from_millis(50);
/// focal-consensus's `election_tick` default.
const ELECTION_TICK: usize = 10;
/// focal-consensus's `heartbeat_tick` default: a leader heartbeats every two ticks.
const HEARTBEAT_TICK: usize = 2;
/// A group's voters.
const VOTERS: u64 = 3;

/// What each replica states (`hyper_raft::Limits::derive`): its group's voters, a message of a UDP
/// datagram's 64 KiB, queues of 16 MiB each, past anything an idle group holds, and one write out.
fn limits() -> hyper_raft::Limits {
    hyper_raft::Limits::derive(hyper_raft::Stated {
        message: 64 << 10,
        members: VOTERS as usize,
        memory: 16 << 20,
        depth: 1,
    })
    .expect("the bench's statement gives its bounds")
}

/// A member's durable state in memory: what the core reads back.
#[derive(Default)]
struct Store {
    hard_state: HardState,
    conf: ConfState,
    entries: Vec<Entry>,
}

impl Storage for Store {
    fn initial_state(&self) -> Result<InitialState, StorageError> {
        Ok(InitialState {
            hard_state: self.hard_state,
            configuration: self.conf.clone(),
            proposals: Vec::new(),
            released: 0,
        })
    }
    fn entries(
        &self,
        low: u64,
        high: u64,
        _max_bytes: u64,
        into: &mut Vec<Entry>,
    ) -> Result<(), StorageError> {
        if low < 1 || high > self.entries.len() as u64 + 1 || low > high {
            return Err(StorageError::Unavailable);
        }
        into.extend_from_slice(&self.entries[(low - 1) as usize..(high - 1) as usize]);
        Ok(())
    }
    fn any_entry(
        &self,
        low: u64,
        high: u64,
        predicate: &mut dyn FnMut(&Entry) -> bool,
    ) -> Result<bool, StorageError> {
        if low < 1 || high > self.entries.len() as u64 + 1 || low > high {
            return Err(StorageError::Unavailable);
        }
        Ok(self.entries[(low - 1) as usize..(high - 1) as usize]
            .iter()
            .any(predicate))
    }
    fn term(&self, index: u64) -> Result<u64, StorageError> {
        if index == 0 {
            return Ok(0);
        }
        self.entries
            .get((index - 1) as usize)
            .map(|entry| entry.term)
            .ok_or(StorageError::Unavailable)
    }
    fn first_index(&self) -> Result<u64, StorageError> {
        Ok(1)
    }
    fn last_index(&self) -> Result<u64, StorageError> {
        Ok(self.entries.len() as u64)
    }
    fn snapshot(&self, _request_index: u64, _to: u64) -> Result<Snapshot, StorageError> {
        Err(StorageError::SnapshotTemporarilyUnavailable)
    }
}

/// One replica: its group, its applied index and its core.
struct Replica {
    group: usize,
    applied: u64,
    raw: RawNode<Store>,
}

/// Every replica of every group, and the messages between them.
struct Groups {
    replicas: Vec<Replica>,
    /// The replica of each group's member `id`, at `id − 1`.
    members: Vec<[usize; VOTERS as usize]>,
    inbox: Vec<(usize, Message)>,
    /// Messages delivered.
    delivered: u64,
}

impl Groups {
    /// `per_node` groups on each of `nodes` nodes, three voters a group, placed round robin.
    fn new(nodes: usize, per_node: usize) -> Self {
        let total = (per_node * nodes).div_ceil(VOTERS as usize);
        let mut replicas = Vec::new();
        let mut members = Vec::new();
        for group in 0..total {
            let mut held = [0usize; VOTERS as usize];
            for (slot, at) in held.iter_mut().enumerate() {
                let store = Store {
                    conf: ConfState {
                        voters: (1..=VOTERS).collect(),
                        ..ConfState::default()
                    },
                    ..Store::default()
                };
                let config = hyper_raft::Config {
                    election_tick: ELECTION_TICK,
                    heartbeat_tick: HEARTBEAT_TICK,
                    check_quorum: true,
                    pre_vote: true,
                    seed: (group * VOTERS as usize + slot) as u64 + 1,
                    ..hyper_raft::Config::new(slot as u64 + 1, limits())
                };
                *at = replicas.len();
                replicas.push(Replica {
                    group,
                    applied: 0,
                    raw: RawNode::new(&config, store).unwrap(),
                });
            }
            members.push(held);
        }
        Self {
            replicas,
            members,
            inbox: Vec::new(),
            delivered: 0,
        }
    }

    /// Every ready drained and every message delivered, until nothing moves.
    fn settle(&mut self) {
        loop {
            for replica in &mut self.replicas {
                while replica.raw.has_ready() {
                    let mut ready = replica.raw.ready().unwrap();
                    let store = replica.raw.store_mut();
                    for entry in ready.entries() {
                        store.entries.truncate((entry.index - 1) as usize);
                        store.entries.push(entry.clone());
                    }
                    if let Some(hard) = ready.hard_state() {
                        store.hard_state = *hard;
                    }
                    let group = replica.group;
                    for message in ready.take_messages() {
                        self.inbox.push((group, message));
                    }
                    for message in ready.take_persisted_messages() {
                        self.inbox.push((group, message));
                    }
                    if let Some(last) = ready.committed_entries().last() {
                        replica.applied = replica.applied.max(last.index);
                    }
                    let mut light = replica.raw.advance_append(ready).unwrap();
                    if let Some(commit) = light.commit_index() {
                        replica.raw.store_mut().hard_state.commit = commit;
                        replica.raw.commit_durable(commit).unwrap();
                    }
                    for message in light.take_messages() {
                        self.inbox.push((group, message));
                    }
                    if let Some(last) = light.committed_entries().last() {
                        replica.applied = replica.applied.max(last.index);
                    }
                    replica.raw.advance_apply_to(replica.applied).unwrap();
                }
            }
            if self.inbox.is_empty() {
                return;
            }
            for (group, message) in std::mem::take(&mut self.inbox) {
                let Some(&at) = self.members[group].get((message.to - 1) as usize) else {
                    continue;
                };
                self.delivered += 1;
                let _ = self.replicas[at].raw.step(message);
            }
        }
    }

    fn tick(&mut self) {
        for replica in &mut self.replicas {
            let _ = replica.raw.tick();
        }
        self.settle();
    }

    fn leaders(&self) -> usize {
        self.replicas
            .iter()
            .filter(|replica| replica.raw.raft.state() == hyper_raft::StateRole::Leader)
            .count()
    }
}

/// Per node per second at `ETA`: messages sent or received, and CPU microseconds.
struct Rate {
    messages: f64,
    cpu_us: f64,
}

#[allow(
    clippy::disallowed_methods,
    reason = "a benchmark measures real time on the host"
)]
fn per_group(nodes: usize, per_node: usize) -> Rate {
    let mut groups = Groups::new(nodes, per_node);
    let total = groups.members.len();
    // Elected: every group has its leader, and a heartbeat round has settled.
    let mut ticks = 0;
    while groups.leaders() < total {
        groups.tick();
        ticks += 1;
        assert!(ticks < 100 * ELECTION_TICK, "groups did not elect");
    }
    for _ in 0..4 * ELECTION_TICK {
        groups.tick();
    }
    let counted = 200usize;
    groups.delivered = 0;
    let started = Instant::now();
    for _ in 0..counted {
        groups.tick();
    }
    let elapsed = started.elapsed();
    assert_eq!(
        groups.leaders(),
        total,
        "a group lost its leader while idle"
    );
    // Each message is sent by one node and received by another.
    let ticks_per_second = HEARTBEAT_TICK as f64 / ETA.as_secs_f64();
    let per_tick_messages = 2.0 * groups.delivered as f64 / counted as f64 / nodes as f64;
    let per_tick_cpu = elapsed.as_secs_f64() * 1e6 / counted as f64 / nodes as f64;
    Rate {
        messages: per_tick_messages * ticks_per_second,
        cpu_us: per_tick_cpu * ticks_per_second,
    }
}

/// The node-pair stream at `ETA`: its measured cost per heartbeat, `P` heartbeats sent and `P`
/// taken a node each `ETA`, and the liveness writes it asked for per heartbeat sent.
fn per_pair(nodes: usize, per_node: usize) -> (Rate, f64) {
    let peers = nodes - 1;
    // Each pair shares the groups the node holds with it: `G · 2 / P` of a node's `G`, each
    // group having two other voters.
    let shared = ((per_node * 2).div_ceil(peers)).max(1) as u32;
    let mut world = World::new(nodes, shared, 0x9E37_79B9_7F4A_7C15);
    world.warm();
    world.sent = 0;
    world.taken = 0;
    world.flushes = 0;
    world.busy = Duration::ZERO;
    world.run(Duration::from_secs(10));
    let beats = (world.sent + world.taken) as f64;
    let per_beat_us = world.busy.as_secs_f64() * 1e6 / beats;
    let per_second = peers as f64 / ETA.as_secs_f64();
    let flush_share = world.flushes as f64 / world.sent as f64 * peers as f64;
    (
        Rate {
            messages: 2.0 * per_second,
            cpu_us: 2.0 * per_second * per_beat_us,
        },
        flush_share,
    )
}

/// An acknowledgement in flight to a prober: who answered, the probe it answers, when it lands,
/// and the relay bound it states.
type Flight = (HostId, u64, u64, u64);

/// slates' detector at `ETA`: a member probes each of its `P` peers once a round of `P` periods.
#[allow(
    clippy::disallowed_methods,
    reason = "a benchmark measures real time on the host"
)]
fn swim(nodes: usize) -> Rate {
    let peers = nodes - 1;
    let mut members: Vec<Detector> = (0..nodes as u64)
        .map(|id| {
            // The simulation's stamps are whole nanoseconds.
            let mut detector = Detector::new(
                HostId(id),
                Exposure::new(),
                NonZeroUsize::new(nodes).unwrap(),
                Duration::from_nanos(1),
                // No detection budget: periods at what their probes need, as the costs in docs/benchmarks.md were measured.
                Duration::ZERO,
            );
            for peer in 0..nodes as u64 {
                detector.join(HostId(peer)).unwrap();
            }
            detector
        })
        .collect();
    let mut now = vec![1u64; nodes];
    // Each member's acknowledgements in flight, as hyper-swim's own bench drives them: who answered
    // which probe, when it lands, and the relay bound the answer states; as many as its detector
    // keeps probe records for, an older answer being one no record credits.
    let mut flights: Vec<[Option<Flight>; OUTSTANDING]> = vec![[None; OUTSTANDING]; nodes];
    // A xorshift stream (Marsaglia 2003), hyper-swim's bench's seed.
    let mut noise = 0x2545_F491_4F6C_DD1Du64;
    let mut requests: Vec<PingReq> = Vec::new();
    let mut messages = 0u64;
    let mut run = |members: &mut Vec<Detector>, periods: usize, messages: &mut u64| {
        for _ in 0..periods {
            for prober in 0..nodes {
                let ping = loop {
                    // A wake comes Linux's 50 µs timer slack late, or the next answer lands first.
                    let landing = flights[prober]
                        .iter()
                        .flatten()
                        .map(|(_, _, lands, _)| *lands)
                        .min();
                    let at = match (members[prober].wake(), landing) {
                        (Some(wake), Some(lands)) => (wake + 50_000).min(lands),
                        (Some(wake), None) => wake + 50_000,
                        (None, Some(lands)) => lands,
                        (None, None) => now[prober],
                    };
                    now[prober] = now[prober].max(at);
                    // The answers that have landed, in the order they land.
                    while let Some(slot) = flights[prober]
                        .iter_mut()
                        .filter(|slot| slot.is_some_and(|(_, _, lands, _)| lands <= now[prober]))
                        .min_by_key(|slot| slot.map(|(_, _, lands, _)| lands))
                    {
                        if let Some((from, nonce, lands, within)) = slot.take() {
                            members[prober].on_ack(from, nonce, lands, within);
                        }
                    }
                    if let Some(ping) = members[prober].poll(now[prober], &mut requests) {
                        break ping;
                    }
                };
                let target = ping.to.0 as usize;
                let ack = members[target].on_ping(HostId(prober as u64));
                // A LAN's round trip, 200 µs and up to half as much again, as hyper-swim's bench.
                noise ^= noise << 13;
                noise ^= noise >> 7;
                noise ^= noise << 17;
                let lands = now[prober] + 200_000 + noise % 100_000;
                // In flight: a free slot, or else the one that lands first, whose answer is lost.
                let slots = &mut flights[prober];
                let at = slots
                    .iter()
                    .position(Option::is_none)
                    .or_else(|| {
                        (0..slots.len()).min_by_key(|at| slots[*at].map(|(_, _, lands, _)| lands))
                    })
                    .unwrap();
                slots[at] = Some((ping.to, ping.nonce, lands, ack.relay_within_ns));
                *messages += 2;
            }
        }
    };
    // Counted once every member judges every peer by an estimator's verdict, the pair's own or the
    // pool's, as it runs for good: not by the pair's timer, which judges a pair before either.
    let judged = |members: &[Detector]| {
        members.iter().enumerate().all(|(id, member)| {
            (0..nodes as u64)
                .filter(|peer| *peer != id as u64)
                .all(|peer| {
                    member
                        .report(HostId(peer))
                        .is_some_and(|report| matches!(report.judge, Judge::Own | Judge::Pool))
                })
        })
    };
    while !judged(&members) {
        run(&mut members, 200 * peers, &mut messages);
    }
    messages = 0;
    let counted = 400 * peers;
    let started = Instant::now();
    run(&mut members, counted, &mut messages);
    let elapsed = started.elapsed();
    // A period at `ETA / P`, so a pair is probed every `ETA`.
    let periods_per_second = peers as f64 / ETA.as_secs_f64();
    Rate {
        // Each message is sent by one member and received by another.
        messages: 2.0 * messages as f64 / counted as f64 / nodes as f64 * periods_per_second,
        cpu_us: elapsed.as_secs_f64() * 1e6 / counted as f64 / nodes as f64 * periods_per_second,
    }
}

fn main() {
    println!(
        "liveness per node per second at a pair interval of {ETA:?}: messages sent and received, \
         and CPU µs"
    );
    println!(
        "  {:>3} {:>6} | {:>10} {:>9} {:>8} | {:>10} {:>9} | {:>10} {:>9}",
        "P",
        "G",
        "pair msgs",
        "pair µs",
        "flushes",
        "group msgs",
        "group µs",
        "swim msgs",
        "swim µs"
    );
    for nodes in [3usize, 9] {
        eprintln!("{nodes} nodes: slates' detector");
        let swim = swim(nodes);
        for per_node in [1usize, 64, 1_024] {
            eprintln!("{nodes} nodes, {per_node} groups a node: the node-pair stream");
            let (pair, flushes) = per_pair(nodes, per_node);
            eprintln!("{nodes} nodes, {per_node} groups a node: per-group heartbeats");
            let group = per_group(nodes, per_node);
            println!(
                "  {:>3} {:>6} | {:>10.0} {:>9.1} {:>8.2} | {:>10.0} {:>9.1} | {:>10.0} {:>9.1}",
                nodes - 1,
                per_node,
                pair.messages,
                pair.cpu_us,
                flushes,
                group.messages,
                group.cpu_us,
                swim.messages,
                swim.cpu_us
            );
        }
    }
}
