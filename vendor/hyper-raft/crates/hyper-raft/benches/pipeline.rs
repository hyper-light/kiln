// Dependency-free bench: a plain `harness = false` binary, no criterion (the
// workspace's deny.toml forbids unmaintained/unvetted deps). A measurement
// tool, not a pass/fail test.
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable,
    clippy::indexing_slicing,
    clippy::disallowed_macros,
    clippy::cast_precision_loss,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cognitive_complexity,
    unreachable_pub
)]
//! Readies in flight against one at a time (core step R-4, `docs/durable.md`
//! §2.1 and §14.1): three voters, each with a device that flushes one batch
//! at a time and takes every write submitted before the flush began into it
//! (group commit), on a network that delivers each message a fixed one-way
//! time later. Closed-loop clients propose 64-byte entries at the leader
//! and propose again once theirs is applied there. Time is simulated, in
//! microseconds, so the latencies are the protocol's and the devices', not
//! this machine's; the wall time per committed entry is the core's and the
//! harness's CPU.
//!
//! Two devices:
//! - **flush**: a flush answers the writes it holds;
//! - **confirmed**: hyper-log's, where a frame is answered once a later
//!   flush has written its persist record (mantle `docs/design/replica.md`
//!   §3), and a frame nothing follows is confirmed by a flush of its own.
//!
//! On both, a write that asks for no flush (`Ready::must_sync`, a commit
//! alone) is answered with the writes before it and costs none.
//!
//! Two commits: **classic**, a quorum's durable answers; and **held**, the
//! voters each its own fate, a held commit's holders and its leader spanning
//! all three (core step R-8, `docs/raft.md` §3.7): a follower's held answer
//! leaves at once under its durable promise, and the leader commits on the
//! first of the classic quorum and a held quorum.
//!
//! The times are this machine's, measured: a flush is 4,330 µs, mantle's
//! p50 for an append that waits one `F_FULLFSYNC` on APFS
//! (`docs/measurements/2026-09-29-log-confirmation.md` in mantle), and a
//! one-way trip is half the 395 µs round trip QUIC measured on loopback
//! (`docs/benchmarks.md`, hyper-transport, "An answer is charged with what
//! arrives"). The depths are one, the owner that finishes each write before
//! it takes the next, and two and three, up to hyper-log's three pipeline
//! frames (`docs/durable.md` §6).
#[path = "../tests/support/mod.rs"]
mod support;

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap},
    time::Instant,
};

use hyper_raft::{
    Config, RawNode,
    proto::{ConfState, Entry, HardState, Message},
};
use support::Store;

/// A flush, in µs (module docs).
const FLUSH: u64 = 4_330;
/// A one-way trip, in µs: half the measured 395 µs round trip.
const ONE_WAY: u64 = 198;
const VOTERS: u64 = 3;

struct Write {
    number: u64,
    sync: bool,
    entries: Vec<Entry>,
    hard: Option<HardState>,
    messages: Vec<Message>,
}

struct Member {
    raw: RawNode<Store>,
    depth: usize,
    /// Submitted, waiting for the next flush.
    queued: Vec<Write>,
    /// In the flush under way.
    flushing: Vec<Write>,
    /// Flushed, waiting for a later flush to confirm them (the confirmed
    /// device).
    unconfirmed: Vec<Write>,
    busy: bool,
    applied: u64,
}

enum Event {
    Deliver(Message),
    Flushed(u64),
}

struct Group {
    members: Vec<Member>,
    confirmed: bool,
    events: BinaryHeap<Reverse<(u64, u64, usize)>>,
    payloads: BTreeMap<usize, Event>,
    next: usize,
    sequence: u64,
    flushes: u64,
    /// When each entry the clients proposed was proposed, by index.
    proposed: BTreeMap<u64, u64>,
    latencies: Vec<u64>,
    /// Clients whose entry was applied, waiting to propose again.
    returning: u64,
}

impl Group {
    fn new(depth: usize, confirmed: bool, held: bool) -> Self {
        let mut members = Vec::new();
        for id in 1..=VOTERS {
            let store = Store::new(ConfState {
                voters: (1..=VOTERS).collect(),
                ..ConfState::default()
            });
            let config = Config {
                election_tick: 10,
                heartbeat_tick: 2,
                max_size_per_msg: 4 * 1024 * 1024 + 1024,
                max_inflight_msgs: 128,
                fates: held.then(|| hyper_raft::Fates {
                    of: (1..=VOTERS).map(|id| (id, id)).collect(),
                    spanning: VOTERS as usize,
                }),
                ..Config::new(id, support::limits(VOTERS as usize, depth))
            };
            members.push(Member {
                raw: RawNode::new(&config, store).unwrap(),
                depth,
                queued: Vec::new(),
                flushing: Vec::new(),
                unconfirmed: Vec::new(),
                busy: false,
                applied: 0,
            });
        }
        Self {
            members,
            confirmed,
            events: BinaryHeap::new(),
            payloads: BTreeMap::new(),
            next: 0,
            sequence: 0,
            flushes: 0,
            proposed: BTreeMap::new(),
            latencies: Vec::new(),
            returning: 0,
        }
    }
    fn at(&mut self, time: u64, event: Event) {
        self.next += 1;
        self.sequence += 1;
        self.payloads.insert(self.next, event);
        self.events.push(Reverse((time, self.sequence, self.next)));
    }
    fn send(&mut self, now: u64, messages: Vec<Message>) {
        for message in messages {
            self.at(now + ONE_WAY, Event::Deliver(message));
        }
    }
    fn applied(&mut self, member: usize, now: u64, entries: &[Entry]) {
        for entry in entries {
            self.members[member].applied = entry.index;
            if member == 0
                && let Some(proposed) = self.proposed.remove(&entry.index)
            {
                self.latencies.push(now - proposed);
                self.returning += 1;
            }
        }
    }
    /// Takes what the member may, submits it, and starts a flush if the
    /// device is idle. What may be applied and sent without a write is first,
    /// whatever the writes out (`RawNode::light_now`).
    fn drive(&mut self, member: usize, now: u64) {
        loop {
            let mut light = self.members[member].raw.light_now(false).unwrap();
            let committed = light.take_committed_entries();
            self.applied(member, now, &committed);
            let node = &mut self.members[member];
            node.raw.advance_apply_to(node.applied).unwrap();
            self.send(now, light.take_messages());
            let node = &mut self.members[member];
            if !node.raw.has_ready() || node.raw.in_flight() >= node.depth {
                break;
            }
            let mut ready = node.raw.ready().unwrap();
            let write = Write {
                number: ready.number(),
                sync: ready.must_sync(),
                entries: ready.take_entries(),
                hard: ready.hard_state().copied(),
                messages: ready.take_persisted_messages(),
            };
            // Held answers leave at once, whatever the write (R-8).
            let mut messages = ready.take_held_messages();
            messages.extend(ready.take_messages());
            let committed = ready.take_committed_entries();
            node.raw.advance_issued(ready).unwrap();
            let idle = !node.busy && node.queued.is_empty() && node.unconfirmed.is_empty();
            node.queued.push(write);
            self.applied(member, now, &committed);
            let node = &mut self.members[member];
            node.raw.advance_apply_to(node.applied).unwrap();
            self.send(now, messages);
            // Nothing before it is out, and it asks for no flush.
            if idle && !self.members[member].queued[0].sync {
                let writes = std::mem::take(&mut self.members[member].queued);
                self.answer(member, now, writes);
                return;
            }
        }
        self.start(member, now);
    }
    /// A flush begins if the device is idle and has something to flush or to
    /// confirm.
    fn start(&mut self, member: usize, now: u64) {
        let node = &mut self.members[member];
        if !node.busy && (!node.queued.is_empty() || !node.unconfirmed.is_empty()) {
            node.flushing = std::mem::take(&mut node.queued);
            node.busy = true;
            self.at(now + FLUSH, Event::Flushed(member as u64));
        }
    }
    fn flushed(&mut self, member: usize, now: u64) {
        self.flushes += 1;
        let confirmed = self.confirmed;
        let node = &mut self.members[member];
        node.busy = false;
        let flushed = std::mem::take(&mut node.flushing);
        let answered = if confirmed {
            // This flush confirmed what the last one wrote, and what it wrote
            // waits for the next.
            let answered = std::mem::replace(&mut node.unconfirmed, flushed);
            if node.unconfirmed.iter().all(|write| !write.sync) {
                // A batch of no flush needs no confirmation of its own.
                let mut answered = answered;
                answered.append(&mut node.unconfirmed);
                answered
            } else {
                answered
            }
        } else {
            flushed
        };
        let mut answered = answered;
        let node = &mut self.members[member];
        if node.unconfirmed.is_empty() {
            // What asks for no flush and follows only what is answered now
            // is answered with it: its order is kept, and it costs nothing.
            let free = node.queued.iter().take_while(|write| !write.sync).count();
            answered.extend(node.queued.drain(..free));
        }
        if !answered.is_empty() {
            self.answer(member, now, answered);
        } else {
            self.drive(member, now);
        }
    }
    /// The writes are durable and answered, in order: the core is told.
    fn answer(&mut self, member: usize, now: u64, writes: Vec<Write>) {
        let node = &mut self.members[member];
        let mut released = Vec::new();
        let mut last = 0;
        for write in writes {
            let disk = &mut node.raw.store_mut().0;
            disk.append(&write.entries);
            if let Some(hard) = write.hard {
                disk.hard_state = hard;
            }
            released.extend(write.messages);
            last = write.number;
        }
        let mut light = node.raw.on_persist(last).unwrap();
        released.extend(light.take_messages());
        let committed = light.take_committed_entries();
        self.applied(member, now, &committed);
        let node = &mut self.members[member];
        node.raw.advance_apply_to(node.applied).unwrap();
        self.send(now, released);
        self.drive(member, now);
    }
    fn propose(&mut self, now: u64) {
        let leader = &mut self.members[0];
        leader.raw.propose(Vec::new(), vec![0xa5; 64]).unwrap();
        let index = leader.raw.raft.log().last_index().unwrap();
        self.proposed.insert(index, now);
    }
    /// Runs until `entries` were applied at the leader; the latencies, the
    /// flushes and the simulated time they took.
    fn run(&mut self, clients: u64, entries: usize) -> u64 {
        let mut now = 0;
        self.members[0].raw.campaign().unwrap();
        self.drive(0, now);
        let mut started = false;
        while self.latencies.len() < entries {
            if !started && self.members[0].raw.raft.state() == hyper_raft::StateRole::Leader {
                // Once the leader's first entry is applied, the clients begin.
                if self.members[0].applied >= 1 {
                    started = true;
                    for _ in 0..clients {
                        self.propose(now);
                    }
                    self.drive(0, now);
                }
            }
            for _ in 0..std::mem::take(&mut self.returning) {
                self.propose(now);
            }
            self.drive(0, now);
            let Some(Reverse((time, _, key))) = self.events.pop() else {
                panic!("nothing left to happen");
            };
            now = time;
            match self.payloads.remove(&key).unwrap() {
                Event::Deliver(message) => {
                    let to = (message.to - 1) as usize;
                    // A refusal changed nothing; the network may lose it.
                    let _ = self.members[to].raw.step(message);
                    self.drive(to, now);
                }
                Event::Flushed(member) => self.flushed(member as usize, now),
            }
        }
        now
    }
}

fn percentile(sorted: &[u64], part: f64) -> u64 {
    sorted[((sorted.len() - 1) as f64 * part) as usize]
}

fn main() {
    for confirmed in [false, true] {
        for held in [false, true] {
            table(confirmed, held);
        }
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "a benchmark measures real time on the host"
)]
fn table(confirmed: bool, held: bool) {
    println!(
        "{:<10} {:<8} {:<8} {:>7} {:>10} {:>10} {:>14} {:>16} {:>12}",
        "device",
        "commit",
        "clients",
        "depth",
        "p50 µs",
        "p99 µs",
        "entries/s",
        "flushes/entry",
        "wall ns/entry"
    );
    let device = if confirmed { "confirmed" } else { "flush" };
    let commit = if held { "held" } else { "classic" };
    let entries = 20_000;
    for clients in [1u64, 4, 16, 64] {
        for depth in [1usize, 2, 3] {
            // The better of three for the wall time; the simulated numbers
            // are the same every run.
            let mut wall = f64::INFINITY;
            let mut outcome = (0, 0, 0.0, 0.0);
            for _ in 0..3 {
                let mut group = Group::new(depth, confirmed, held);
                let started = Instant::now();
                let simulated = group.run(clients, entries);
                let elapsed = started.elapsed().as_nanos() as f64;
                wall = wall.min(elapsed / entries as f64);
                let mut latencies = group.latencies.clone();
                latencies.sort_unstable();
                outcome = (
                    percentile(&latencies, 0.5),
                    percentile(&latencies, 0.99),
                    entries as f64 / (simulated as f64 / 1e6),
                    group.flushes as f64 / entries as f64,
                );
            }
            println!(
                "{:<10} {:<8} {:<8} {:>7} {:>10} {:>10} {:>14.0} {:>16.3} {:>12.0}",
                device, commit, clients, depth, outcome.0, outcome.1, outcome.2, outcome.3, wall
            );
        }
    }
}
