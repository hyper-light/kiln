//! What a step of `hyper-sim`'s world costs against the harnesses it will replace
//! (`docs/sim.md` §10; `docs/benchmarks.md`, "hyper-sim's world against the harnesses"):
//! `cargo bench -p hyper-sim --bench step -- [rounds]`.
//!
//! Three workloads, each run on the replaced harness's own machinery and on the world, with the
//! same process logic, the same draws a step and the same payloads:
//! - **untimed**: hyper-raft's `tests/support` schedule (`Cluster::choose`/`act`): SplitMix64 with
//!   its multiply-shift `below`, a `Vec` network of 144-byte messages taken at a drawn index with
//!   `Vec::remove`, the oldest dropped at the bound; against the world's free discipline with
//!   [`Random`]. Each step delivers one message, draws its loss, and the receiver sends one to a
//!   peer it draws, so the messages in flight stay at the population (16, 256 and 2,048, the
//!   harness's bound).
//! - **timed**: hyper-liveness's `tests/sim.rs` world: a `BinaryHeap` of `(time, key)` with the
//!   events in a `BTreeMap`, a xorshift with `% span`, each node's wake found by a scan with its
//!   lateness drawn at every turn of the loop; against the world's ordered discipline. Every
//!   node heartbeats every peer each 10 ms with LAN delays, stalls and loss (the test's `LAN`),
//!   payloads of 64 bytes held inline; 3, 8 and 64 nodes.
//! - **liveness**: hyper-liveness itself on its test's harness (`Sim`, copied with its assertions'
//!   records left out) and on the world: three nodes configured, then a minute of virtual time.
//!
//! Per step (an event or a wake run; for hyper-liveness, per heartbeat sent): nanoseconds, allocations, reallocations and page faults, the
//! counting from after a warm-up. The variants run in an order rotated by one each round, and
//! each round prints the one-minute load average read just before it.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
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

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::time::Instant;

use hyper_measure::{alloc, faults};
use hyper_sim::{Clock, Discipline, Lateness, Limits, NodeId, Random, Source, Step, World};

#[global_allocator]
static ALLOCATOR: alloc::Counting = alloc::Counting;

const MS: u64 = 1_000_000;
const US: u64 = 1_000;

/// The host clock, for the benchmark's own timing.
#[allow(
    clippy::disallowed_methods,
    reason = "a benchmark times itself on the host clock"
)]
fn host_now() -> Instant {
    Instant::now()
}

/// The one-minute load average: `/proc/loadavg` on Linux, `vm.loadavg` on macOS.
fn load() -> f64 {
    let text = std::fs::read_to_string("/proc/loadavg").ok().or_else(|| {
        std::process::Command::new("sysctl")
            .args(["-n", "vm.loadavg"])
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
    });
    text.and_then(|t| {
        t.split_whitespace()
            .find_map(|word| word.parse::<f64>().ok())
    })
    .unwrap_or(f64::NAN)
}

/// One measured run: steps taken and a checksum the optimizer cannot drop.
struct Run {
    steps: u64,
    sum: u64,
}

/// What a run cost a step.
#[derive(Clone, Copy, Default)]
struct Cost {
    ns: f64,
    allocs: f64,
    reallocs: f64,
    faults: f64,
    /// Of the allocations, those the event machinery made (counted where a harness sets its
    /// queue's and world's calls aside: the hyper-liveness pair).
    machinery: f64,
}

/// `body` measured: `warm` runs first, uncounted; then `measure` is counted.
fn measure<S>(warm: impl FnOnce() -> S, body: impl FnOnce(&mut S) -> Run) -> Cost {
    let mut state = warm();
    let before = faults::read().unwrap();
    alloc::begin();
    let start = host_now();
    let run = body(&mut state);
    let elapsed = start.elapsed();
    let machinery = alloc::read_aside();
    let counts = alloc::end();
    let after = faults::read().unwrap();
    std::hint::black_box(run.sum);
    drop(state);
    let steps = run.steps.max(1) as f64;
    Cost {
        ns: elapsed.as_nanos() as f64 / steps,
        allocs: counts.allocations as f64 / steps,
        reallocs: counts.reallocations as f64 / steps,
        faults: after.since(&before).total() as f64 / steps,
        machinery: (machinery.allocations + machinery.reallocations) as f64 / steps,
    }
}

// ---------------------------------------------------------------- untimed

/// A message of hyper-raft's size: 144 bytes (`docs/benchmarks.md`, "The message's layout").
#[derive(Clone, Copy)]
struct Msg {
    from: u64,
    to: u64,
    body: [u64; 16],
}

const NODES_UNTIMED: u64 = 3;
/// hyper-raft's `tests/support/cluster.rs` network bound.
const NETWORK: usize = 2048;
const STEPS: u64 = 200_000;

/// hyper-raft's `Seeded`, as its support has it.
struct RaftSeeded(u64);
impl RaftSeeded {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut drawn = self.0;
        drawn = (drawn ^ (drawn >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        drawn = (drawn ^ (drawn >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        drawn ^ (drawn >> 31)
    }
    fn below(&mut self, bound: u64) -> u64 {
        ((u128::from(self.next()) * u128::from(bound)) >> 64) as u64
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

fn message(from: u64, to: u64, step: u64) -> Msg {
    Msg {
        from,
        to,
        body: [step; 16],
    }
}

/// The receiver's work, the same in both: fold the message and draw where its answer goes.
fn receive(msg: &Msg, peer_draw: u64) -> (u64, u64) {
    let to = (msg.to + 1 + peer_draw) % NODES_UNTIMED;
    (msg.to, to)
}

struct RaftHarness {
    rng: RaftSeeded,
    net: Vec<Msg>,
}

fn raft_warm(population: usize) -> RaftHarness {
    let mut h = RaftHarness {
        rng: RaftSeeded(17),
        net: Vec::new(),
    };
    for i in 0..population as u64 {
        h.net.push(message(i % 3, (i + 1) % 3, i));
    }
    raft_steps(&mut h, STEPS / 10);
    h
}

fn raft_steps(h: &mut RaftHarness, steps: u64) -> Run {
    let mut sum = 0u64;
    for step in 0..steps {
        // `Op::Deliver { at, lose }` as `choose` draws it, and `act` takes it.
        let at = h.rng.below(h.net.len() as u64) as usize;
        let lose = h.rng.chance(1);
        let msg = h.net.remove(at);
        if !lose {
            sum = sum.wrapping_add(msg.body[0] ^ msg.from);
        }
        let (from, to) = receive(&msg, h.rng.below(NODES_UNTIMED - 1));
        if h.net.len() >= NETWORK {
            h.net.remove(0);
        }
        h.net.push(message(from, to, step));
    }
    Run { steps, sum }
}

struct WorldUntimed {
    world: World<Msg>,
    nodes: Vec<NodeId>,
    links: Vec<hyper_sim::StreamId>,
    own: Vec<hyper_sim::StreamId>,
}

/// The bounds of a synthetic run: `words` decisions a step at most, over the warm-up and the
/// measured steps.
fn world_limits(events: usize, nodes: usize, words: usize) -> Limits {
    let steps = STEPS + STEPS / 10;
    Limits {
        events,
        nodes,
        streams: 2 + 2 * nodes + nodes * nodes,
        steps,
        trace_words: steps as usize * words,
    }
}

fn world_untimed_warm(population: usize) -> WorldUntimed {
    let mut world = World::new(
        Source::Seed(17),
        Discipline::Free,
        // A step draws the event, its loss and the receiver's peer.
        world_limits(NETWORK, NODES_UNTIMED as usize, 3),
    )
    .unwrap();
    let nodes: Vec<NodeId> = (0..NODES_UNTIMED)
        .map(|_| world.node(Clock::default()).unwrap())
        .collect();
    let own = (0..NODES_UNTIMED)
        .map(|i| world.stream("node", &[i]).unwrap())
        .collect();
    let mut links = Vec::new();
    for from in 0..NODES_UNTIMED {
        for to in 0..NODES_UNTIMED {
            links.push(world.stream("link", &[from, to]).unwrap());
        }
    }
    for i in 0..population as u64 {
        let to = (i + 1) % 3;
        world
            .schedule(0, nodes[to as usize], message(i % 3, to, i))
            .unwrap();
    }
    let mut h = WorldUntimed {
        world,
        nodes,
        links,
        own,
    };
    world_untimed_steps(&mut h, STEPS / 10);
    h
}

fn world_untimed_steps(h: &mut WorldUntimed, steps: u64) -> Run {
    let mut sum = 0u64;
    let mut random = Random;
    for step in 0..steps {
        let Step::Event { event: msg, .. } = h.world.next(&mut random).unwrap() else {
            panic!("the population is never empty");
        };
        let link = h.links[(msg.from * NODES_UNTIMED + msg.to) as usize];
        if !h.world.chance(link, 10_000).unwrap() {
            sum = sum.wrapping_add(msg.body[0] ^ msg.from);
        }
        let peer = h
            .world
            .below(h.own[msg.to as usize], NODES_UNTIMED - 1)
            .unwrap();
        let (from, to) = receive(&msg, peer);
        h.world
            .schedule(h.world.now(), h.nodes[to as usize], message(from, to, step))
            .unwrap();
    }
    Run { steps, sum }
}

// ---------------------------------------------------------------- timed

/// hyper-liveness's test `LAN`: one-way delay, stalls, loss, wake lateness.
const DELAY: (u64, u64) = (80 * US, 60 * US);
const STALL: (f64, u64) = (0.002, 20 * MS);
const LOSS: f64 = 0.001;
const LATE: (u64, u64) = (20 * US, 60 * US);
/// The interval each node heartbeats every peer at.
const BEAT: u64 = 10 * MS;

/// hyper-liveness's test noise: xorshift (Marsaglia 2003), `% span`.
#[derive(Clone)]
struct Noise(u64);
impl Noise {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, span: u64) -> u64 {
        self.next() % span.max(1)
    }
    fn chance(&mut self, p: f64) -> bool {
        ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < p
    }
}

#[derive(Clone, Copy)]
struct Beat {
    from: u64,
    bytes: [u8; 64],
}

struct LivenessHarness {
    now: u64,
    noise: Noise,
    beat_at: Vec<u64>,
    queue: BinaryHeap<Reverse<(u64, u64)>>,
    events: BTreeMap<u64, (usize, Beat)>,
    next_event: u64,
}

impl LivenessHarness {
    fn schedule(&mut self, at: u64, to: usize, beat: Beat) {
        let key = self.next_event;
        self.next_event += 1;
        self.queue.push(Reverse((at, key)));
        self.events.insert(key, (to, beat));
    }
    fn next_wake(&mut self) -> Option<(u64, usize)> {
        let mut best: Option<(u64, usize)> = None;
        for (i, at) in self.beat_at.iter().enumerate() {
            if best.is_none_or(|(b, _)| *at < b) {
                best = Some((*at, i));
            }
        }
        best.map(|(at, i)| (at.max(self.now) + LATE.0 + self.noise.below(LATE.1), i))
    }
}

fn liveness_warm(nodes: usize) -> LivenessHarness {
    let mut h = LivenessHarness {
        now: 0,
        noise: Noise(17 | 1),
        beat_at: (0..nodes as u64).map(|i| i * 997 * US % BEAT).collect(),
        queue: BinaryHeap::new(),
        events: BTreeMap::new(),
        next_event: 0,
    };
    liveness_steps(&mut h, STEPS / 10);
    h
}

fn liveness_steps(h: &mut LivenessHarness, steps: u64) -> Run {
    let nodes = h.beat_at.len();
    let mut sum = 0u64;
    let mut taken = 0;
    while taken < steps {
        let event_at = h.queue.peek().map(|Reverse((at, _))| *at);
        let wake = h.next_wake();
        let is_event = match (event_at, wake) {
            (Some(e), Some((w, _))) => e <= w,
            (Some(_), None) => true,
            (None, _) => false,
        };
        taken += 1;
        if is_event {
            let Reverse((at, key)) = h.queue.pop().unwrap();
            h.now = at;
            let (to, beat) = h.events.remove(&key).unwrap();
            sum = sum.wrapping_add(beat.from ^ to as u64 ^ u64::from(beat.bytes[0]));
        } else if let Some((at, node)) = wake {
            h.now = at;
            for peer in (0..nodes).filter(|p| *p != node) {
                if h.noise.chance(LOSS) {
                    continue;
                }
                let mut delay = DELAY.0 + h.noise.below(DELAY.1);
                if h.noise.chance(STALL.0) {
                    delay += h.noise.below(STALL.1);
                }
                let beat = Beat {
                    from: node as u64,
                    bytes: [taken as u8; 64],
                };
                h.schedule(h.now + delay, peer, beat);
            }
            h.beat_at[node] = h.now + BEAT;
        }
    }
    Run { steps, sum }
}

struct WorldTimed {
    world: World<Beat>,
    nodes: Vec<NodeId>,
    links: Vec<hyper_sim::StreamId>,
}

fn world_timed_warm(count: usize) -> WorldTimed {
    // Every node's heartbeats to every peer, in flight at once at the most.
    let events = count * count * 4;
    let mut world = World::new(
        Source::Seed(17),
        Discipline::Ordered,
        // A wake draws its lateness and at most four a peer (loss, delay, stall, its length), and
        // is one step of `count` (its `count − 1` heartbeats are a step each): at most
        // `1 + 4 (count − 1)` words in `count` steps, under four a step.
        world_limits(events, count, 4),
    )
    .unwrap();
    let late = Lateness {
        floor_ns: LATE.0,
        spread_ns: LATE.1,
    };
    let nodes: Vec<NodeId> = (0..count)
        .map(|_| {
            world
                .node(Clock {
                    lateness: late,
                    ..Clock::default()
                })
                .unwrap()
        })
        .collect();
    let mut links = Vec::new();
    for from in 0..count as u64 {
        for to in 0..count as u64 {
            links.push(world.stream("link", &[from, to]).unwrap());
        }
    }
    for (i, node) in nodes.iter().enumerate() {
        world.wake(*node, Some(i as u64 * 997 * US % BEAT)).unwrap();
    }
    let mut h = WorldTimed {
        world,
        nodes,
        links,
    };
    world_timed_steps(&mut h, STEPS / 10);
    h
}

fn world_timed_steps(h: &mut WorldTimed, steps: u64) -> Run {
    let count = h.nodes.len();
    let mut sum = 0u64;
    let mut random = Random;
    for taken in 0..steps {
        match h.world.next(&mut random).unwrap() {
            Step::Event { node, event } => {
                sum = sum.wrapping_add(event.from ^ u64::from(node.0) ^ u64::from(event.bytes[0]));
            }
            Step::Wake { node } => {
                let from = node.0 as usize;
                for peer in (0..count).filter(|p| *p != from) {
                    let link = h.links[from * count + peer];
                    if h.world.chance(link, 1_000).unwrap() {
                        continue;
                    }
                    let mut delay = DELAY.0 + h.world.below(link, DELAY.1).unwrap();
                    if h.world.chance(link, 2_000).unwrap() {
                        delay += h.world.below(link, STALL.1).unwrap();
                    }
                    let beat = Beat {
                        from: from as u64,
                        bytes: [taken as u8; 64],
                    };
                    h.world.after(delay, h.nodes[peer], beat).unwrap();
                }
                let next = h.world.monotonic(node).unwrap() + BEAT;
                h.world.wake(node, Some(next)).unwrap();
            }
            Step::Idle | Step::Spent => panic!("heartbeats never stop"),
        }
    }
    Run { steps, sum }
}

// ---------------------------------------------------------------- liveness

mod live {
    //! hyper-liveness's `tests/sim.rs` harness, and the same harness on the world.

    use std::cmp::Reverse;
    use std::collections::{BTreeMap, BinaryHeap};
    use std::time::Duration;

    use hyper_liveness::{Change, Liveness, Output, PeerId, Settings, Write};
    use hyper_sim::{
        Clock, Discipline, Lateness, Limits, NodeId, Random, Source, Step, StreamId, World,
    };
    use hyper_timing::{Ballot, Exposure};

    use super::{MS, US, alloc};

    pub(super) const NODES: usize = 3;
    const STEPS_BOUND: u64 = 1 << 15;

    pub(super) struct Owner {
        pub(super) sent: Vec<(PeerId, Vec<u8>)>,
        pub(super) flush: bool,
        pub(super) changes: Vec<Change>,
    }

    impl Output for Owner {
        fn heartbeat(&mut self, peer: PeerId, message: &[u8]) {
            self.sent.push((peer, message.to_vec()));
        }
        fn flush(&mut self) {
            self.flush = true;
        }
        fn change(&mut self, change: Change) {
            self.changes.push(change);
        }
    }

    pub(super) struct Node {
        pub(super) id: PeerId,
        pub(super) liveness: Liveness,
        pub(super) owner: Owner,
        pub(super) disk_busy_until: u64,
        pub(super) suspicions: u64,
        /// Heartbeats sent: the unit the two harnesses are compared in, since their schedules
        /// (drawn from different generators) differ in how many steps a heartbeat takes.
        pub(super) sent: u64,
        /// The wake the owner's timer was set to, and whether it was set before it: the wait that
        /// reaches the wake, by the timer or by what came after it, is then reported to the stream
        /// (`Liveness::on_wait`).
        pub(super) armed: Option<(u64, bool)>,
    }

    /// `node`'s owner woken at `now` by what came: a wait it began before the stream's wake that
    /// this ends at or past the wake is reported, whatever ended it (`Liveness::on_wait`), and
    /// its timer is spent.
    fn woken(node: &mut Node, now: u64) {
        if let Some((deadline, began_before)) = node.armed
            && deadline <= now
        {
            node.armed = None;
            if began_before {
                node.liveness.on_wait(deadline, now);
            }
        }
    }

    fn nodes() -> Vec<Node> {
        (0..NODES)
            .map(|i| {
                let id = i as u64 + 1;
                let mut liveness = Liveness::new(Settings {
                    local: id,
                    // Each node's first start.
                    run: 1,
                    max_peers: NODES,
                    history: Exposure::new(),
                    // The simulation's clocks read whole nanoseconds.
                    resolution: Duration::from_nanos(1),
                })
                .unwrap();
                for peer in 1..=NODES as u64 {
                    if peer != id {
                        liveness.attach(peer).unwrap();
                    }
                }
                Node {
                    id,
                    liveness,
                    owner: Owner {
                        sent: Vec::new(),
                        flush: false,
                        changes: Vec::new(),
                    },
                    disk_busy_until: 0,
                    suspicions: 0,
                    sent: 0,
                    armed: None,
                }
            })
            .collect()
    }

    /// The election law over the measured round trips, as the test's `Sim::elect`.
    fn elect(nodes: &mut [Node]) {
        let count = nodes.len();
        for node in nodes {
            let (Some(granularity), Some(durable)) =
                (node.liveness.granularity(), node.liveness.flush_mean())
            else {
                continue;
            };
            let peers: Vec<PeerId> = (1..=count as u64).filter(|p| *p != node.id).collect();
            let paths: Vec<_> = peers
                .iter()
                .filter_map(|peer| node.liveness.round_trip(*peer).copied())
                .collect();
            let Some(span) = Ballot::measure(paths.iter(), count, durable, granularity)
                .and_then(|ballot| ballot.span(granularity))
            else {
                continue;
            };
            for peer in peers {
                node.liveness.set_election(peer, span.election).unwrap();
            }
        }
    }

    fn configured(nodes: &[Node]) -> bool {
        nodes.iter().all(|n| {
            (1..=nodes.len() as u64)
                .filter(|p| *p != n.id)
                .all(|p| n.liveness.report(p).is_some_and(|r| r.configured))
        })
    }

    enum Event {
        Arrive {
            to: usize,
            from: usize,
            bytes: Vec<u8>,
        },
        Durable {
            node: usize,
            write: Write,
            started: u64,
        },
    }

    /// The test's `Sim`, its assertion records left out.
    pub(super) struct Sim {
        pub(super) now: u64,
        noise: super::Noise,
        pub(super) nodes: Vec<Node>,
        queue: BinaryHeap<Reverse<(u64, u64)>>,
        events: BTreeMap<u64, Event>,
        next_event: u64,
        elected_at: u64,
        pub(super) steps: u64,
    }

    impl Sim {
        pub(super) fn new(seed: u64) -> Self {
            Self {
                now: 0,
                noise: super::Noise(seed | 1),
                nodes: nodes(),
                queue: BinaryHeap::new(),
                events: BTreeMap::new(),
                next_event: 0,
                elected_at: 0,
                steps: 0,
            }
        }
        fn schedule(&mut self, at: u64, event: Event) {
            alloc::aside();
            let key = self.next_event;
            self.next_event += 1;
            self.queue.push(Reverse((at, key)));
            self.events.insert(key, event);
            alloc::back();
        }
        fn submit(&mut self, node: usize, write: Write) {
            let start = self.now.max(self.nodes[node].disk_busy_until);
            let done = start + 200 * US + self.noise.below(400 * US);
            self.nodes[node].disk_busy_until = done;
            let started = self.now;
            self.schedule(
                done,
                Event::Durable {
                    node,
                    write,
                    started,
                },
            );
        }
        fn poll(&mut self, node: usize) {
            let now = self.now;
            let n = &mut self.nodes[node];
            n.liveness.poll(now, &mut n.owner);
            self.drain(node);
        }
        fn drain(&mut self, node: usize) {
            let sent = std::mem::take(&mut self.nodes[node].owner.sent);
            self.nodes[node].sent += sent.len() as u64;
            for (peer, bytes) in sent {
                if self.noise.chance(super::LOSS) {
                    continue;
                }
                let mut delay = super::DELAY.0 + self.noise.below(super::DELAY.1);
                if self.noise.chance(super::STALL.0) {
                    delay += self.noise.below(super::STALL.1);
                }
                let to = peer as usize - 1;
                self.schedule(
                    self.now + delay,
                    Event::Arrive {
                        to,
                        from: node,
                        bytes,
                    },
                );
            }
            if std::mem::take(&mut self.nodes[node].owner.flush) {
                self.submit(node, Write::Liveness);
            }
            for change in std::mem::take(&mut self.nodes[node].owner.changes) {
                if let Change::Suspected(_) = change {
                    self.nodes[node].suspicions += 1;
                }
            }
        }
        fn next_wake(&mut self) -> Option<(u64, usize)> {
            let mut best: Option<(u64, usize)> = None;
            for (i, node) in self.nodes.iter().enumerate() {
                if let Some(at) = node.liveness.wake()
                    && best.is_none_or(|(b, _)| at < b)
                {
                    best = Some((at, i));
                }
            }
            best.map(|(at, i)| {
                // The timer is set at the present: before its deadline, or past it.
                self.nodes[i].armed = Some((at, at >= self.now));
                (
                    at.max(self.now) + super::LATE.0 + self.noise.below(super::LATE.1),
                    i,
                )
            })
        }
        pub(super) fn run(&mut self, until: u64) {
            for node in 0..self.nodes.len() {
                self.poll(node);
            }
            loop {
                let event_at = self.queue.peek().map(|Reverse((at, _))| *at);
                let wake = self.next_wake();
                let (at, is_event) = match (event_at, wake) {
                    (Some(e), Some((w, _))) if e <= w => (e, true),
                    (Some(e), None) => (e, true),
                    (_, Some((w, _))) => (w, false),
                    (None, None) => break,
                };
                if at > until {
                    self.now = until;
                    break;
                }
                self.now = at;
                self.steps += 1;
                if is_event {
                    alloc::aside();
                    let Reverse((_, key)) = self.queue.pop().unwrap();
                    let event = self.events.remove(&key).unwrap();
                    alloc::back();
                    self.handle(event);
                } else if let Some((_, node)) = wake {
                    let now = self.now;
                    let n = &mut self.nodes[node];
                    if let Some((deadline, true)) = n.armed.take() {
                        n.liveness.on_wait(deadline, now);
                    }
                    self.poll(node);
                }
                if self.now >= self.elected_at {
                    elect(&mut self.nodes);
                    self.elected_at = self.now + 100 * MS;
                }
            }
        }
        fn handle(&mut self, event: Event) {
            match event {
                Event::Arrive { to, from, bytes } => {
                    let now = self.now;
                    let n = &mut self.nodes[to];
                    woken(n, now);
                    let _ = n
                        .liveness
                        .on_heartbeat(from as u64 + 1, &bytes, now, &mut n.owner);
                    n.liveness.poll(now, &mut n.owner);
                    self.drain(to);
                }
                Event::Durable {
                    node,
                    write,
                    started,
                } => {
                    let now = self.now;
                    let n = &mut self.nodes[node];
                    woken(n, now);
                    n.liveness.on_durable(write, started, now);
                    n.liveness.poll(now, &mut n.owner);
                    self.drain(node);
                }
            }
        }
        pub(super) fn run_until_configured(&mut self) {
            while !configured(&self.nodes) {
                assert!(self.now < 120_000 * MS);
                let until = self.now + 50 * MS;
                self.run(until);
            }
        }
    }

    /// The same harness on the world: the queue, the clock, the wakes and the draws are the
    /// world's; the process logic is the test's.
    pub(super) struct OnWorld {
        world: World<Event>,
        ids: Vec<NodeId>,
        pub(super) nodes: Vec<Node>,
        links: Vec<StreamId>,
        disks: Vec<StreamId>,
        elected_at: u64,
    }

    impl OnWorld {
        pub(super) fn new(seed: u64) -> Self {
            let limits = Limits {
                // Each pair's heartbeats in flight and each node's flushes: at most a few each.
                events: 4_096,
                nodes: NODES,
                streams: 1 + NODES * 2 + NODES * NODES,
                // Configuration and a minute took 11,687 steps (measured, 2026-10-02); the bound
                // is the next power of two above twice that.
                steps: STEPS_BOUND,
                // A step's decisions: an arrival's heartbeats to the two peers at four words each
                // (loss, delay, stall, its length), a flush's one and the timer's lateness.
                trace_words: STEPS_BOUND as usize * 10,
            };
            let mut world = World::new(Source::Seed(seed), Discipline::Ordered, limits).unwrap();
            let late = Lateness {
                floor_ns: super::LATE.0,
                spread_ns: super::LATE.1,
            };
            let ids = (0..NODES)
                .map(|_| {
                    world
                        .node(Clock {
                            lateness: late,
                            ..Clock::default()
                        })
                        .unwrap()
                })
                .collect();
            let mut links = Vec::new();
            for from in 0..NODES as u64 {
                for to in 0..NODES as u64 {
                    links.push(world.stream("link", &[from, to]).unwrap());
                }
            }
            let disks = (0..NODES as u64)
                .map(|n| world.stream("disk", &[n]).unwrap())
                .collect();
            Self {
                world,
                ids,
                nodes: nodes(),
                links,
                disks,
                elected_at: 0,
            }
        }
        pub(super) fn steps(&self) -> u64 {
            self.world.steps()
        }
        fn submit(&mut self, node: usize, write: Write) {
            let now = self.world.now();
            let start = now.max(self.nodes[node].disk_busy_until);
            alloc::aside();
            let done = start + 200 * US + self.world.below(self.disks[node], 400 * US).unwrap();
            self.nodes[node].disk_busy_until = done;
            self.world
                .schedule(
                    done,
                    self.ids[node],
                    Event::Durable {
                        node,
                        write,
                        started: now,
                    },
                )
                .unwrap();
            alloc::back();
        }
        fn poll(&mut self, node: usize) {
            let now = self.world.now();
            let n = &mut self.nodes[node];
            n.liveness.poll(now, &mut n.owner);
            self.drain(node);
        }
        fn drain(&mut self, node: usize) {
            let sent = std::mem::take(&mut self.nodes[node].owner.sent);
            self.nodes[node].sent += sent.len() as u64;
            for (peer, bytes) in sent {
                let to = peer as usize - 1;
                let link = self.links[node * NODES + to];
                alloc::aside();
                if self.world.chance(link, 1_000).unwrap() {
                    alloc::back();
                    continue;
                }
                let mut delay = super::DELAY.0 + self.world.below(link, super::DELAY.1).unwrap();
                if self.world.chance(link, 2_000).unwrap() {
                    delay += self.world.below(link, super::STALL.1).unwrap();
                }
                self.world
                    .after(
                        delay,
                        self.ids[to],
                        Event::Arrive {
                            to,
                            from: node,
                            bytes,
                        },
                    )
                    .unwrap();
                alloc::back();
            }
            if std::mem::take(&mut self.nodes[node].owner.flush) {
                self.submit(node, Write::Liveness);
            }
            for change in std::mem::take(&mut self.nodes[node].owner.changes) {
                if let Change::Suspected(_) = change {
                    self.nodes[node].suspicions += 1;
                }
            }
            let wake = self.nodes[node].liveness.wake();
            let now = self.world.now();
            self.nodes[node].armed = wake.map(|at| (at, at >= now));
            alloc::aside();
            self.world.wake(self.ids[node], wake).unwrap();
            alloc::back();
        }
        pub(super) fn run(&mut self, until: u64) {
            for node in 0..NODES {
                self.poll(node);
            }
            let mut random = Random;
            loop {
                // A step past `until` waits for the next run, as the test's loop.
                match self.world.earliest() {
                    Some(at) if at <= until => {}
                    _ => {
                        self.world.advance(until).unwrap();
                        break;
                    }
                }
                alloc::aside();
                let step = self.world.next(&mut random).unwrap();
                alloc::back();
                match step {
                    Step::Event { event, .. } => self.handle(event),
                    Step::Wake { node } => {
                        let (now, node) = (self.world.now(), node.0 as usize);
                        let n = &mut self.nodes[node];
                        if let Some((deadline, true)) = n.armed.take() {
                            n.liveness.on_wait(deadline, now);
                        }
                        self.poll(node);
                    }
                    Step::Idle | Step::Spent => break,
                }
                let now = self.world.now();
                if now >= self.elected_at {
                    elect(&mut self.nodes);
                    self.elected_at = now + 100 * MS;
                }
            }
        }
        fn handle(&mut self, event: Event) {
            let now = self.world.now();
            match event {
                Event::Arrive { to, from, bytes } => {
                    let n = &mut self.nodes[to];
                    woken(n, now);
                    let _ = n
                        .liveness
                        .on_heartbeat(from as u64 + 1, &bytes, now, &mut n.owner);
                    n.liveness.poll(now, &mut n.owner);
                    self.drain(to);
                }
                Event::Durable {
                    node,
                    write,
                    started,
                } => {
                    let n = &mut self.nodes[node];
                    woken(n, now);
                    n.liveness.on_durable(write, started, now);
                    n.liveness.poll(now, &mut n.owner);
                    self.drain(node);
                }
            }
        }
    }

    impl OnWorld {
        pub(super) fn run_until_configured(&mut self) {
            while !configured(&self.nodes) {
                assert!(self.world.now() < 120_000 * MS);
                let until = self.world.now() + 50 * MS;
                self.run(until);
            }
        }
        pub(super) fn now(&self) -> u64 {
            self.world.now()
        }
    }
}

// ---------------------------------------------------------------- the runs

/// A minute of virtual time, measured after the three nodes are configured.
const MINUTE: u64 = 60_000 * MS;

fn live_harness() -> Cost {
    measure(
        || {
            let mut sim = live::Sim::new(0x9E37_79B9);
            sim.run_until_configured();
            sim
        },
        |sim| {
            let before: u64 = sim.nodes.iter().map(|n| n.sent).sum();
            let end = sim.now + MINUTE;
            sim.run(end);
            let sum = sim.nodes.iter().map(|n| n.suspicions).sum::<u64>() ^ sim.steps;
            Run {
                steps: sim.nodes.iter().map(|n| n.sent).sum::<u64>() - before,
                sum,
            }
        },
    )
}

fn live_world() -> Cost {
    measure(
        || {
            let mut sim = live::OnWorld::new(0x9E37_79B9);
            sim.run_until_configured();
            sim
        },
        |sim| {
            let before: u64 = sim.nodes.iter().map(|n| n.sent).sum();
            let end = sim.now() + MINUTE;
            sim.run(end);
            let sum = sim.nodes.iter().map(|n| n.suspicions).sum::<u64>() ^ sim.steps();
            Run {
                steps: sim.nodes.iter().map(|n| n.sent).sum::<u64>() - before,
                sum,
            }
        },
    )
}

type Variant = (String, Box<dyn Fn() -> Cost>);

fn variants() -> Vec<Variant> {
    let mut all: Vec<Variant> = Vec::new();
    for population in [16usize, 256, NETWORK] {
        all.push((
            format!("untimed {population} in flight, hyper-raft's support"),
            Box::new(move || measure(|| raft_warm(population), |h| raft_steps(h, STEPS))),
        ));
        all.push((
            format!("untimed {population} in flight, world (free)"),
            Box::new(move || {
                measure(
                    || world_untimed_warm(population),
                    |h| world_untimed_steps(h, STEPS),
                )
            }),
        ));
    }
    for nodes in [3usize, 8, 64] {
        all.push((
            format!("timed {nodes} nodes, hyper-liveness's sim"),
            Box::new(move || measure(|| liveness_warm(nodes), |h| liveness_steps(h, STEPS))),
        ));
        all.push((
            format!("timed {nodes} nodes, world (ordered)"),
            Box::new(move || measure(|| world_timed_warm(nodes), |h| world_timed_steps(h, STEPS))),
        ));
    }
    all.push((
        "hyper-liveness 3 nodes a heartbeat, its sim".to_string(),
        Box::new(live_harness),
    ));
    all.push((
        "hyper-liveness 3 nodes a heartbeat, world (ordered)".to_string(),
        Box::new(live_world),
    ));
    all
}

fn main() {
    assert!(
        alloc::installed(),
        "the counting allocator is not installed"
    );
    let rounds: usize = std::env::args()
        .skip(1)
        .find_map(|arg| arg.parse().ok())
        .unwrap_or(5);
    // Any other argument keeps only the variants whose names contain it.
    let filter: Vec<String> = std::env::args()
        .skip(1)
        .filter(|arg| arg.parse::<usize>().is_err() && !arg.starts_with("--"))
        .collect();
    let all: Vec<Variant> = variants()
        .into_iter()
        .filter(|(name, _)| filter.iter().all(|f| name.contains(f.as_str())))
        .collect();
    let mut costs: Vec<Vec<Cost>> = vec![Vec::new(); all.len()];
    let mut loads = Vec::new();
    for round in 0..rounds {
        let load = load();
        loads.push(load);
        println!("round {round}: load {load:.1}");
        for k in 0..all.len() {
            let at = (k + round) % all.len();
            let cost = (all[at].1)();
            println!(
                "  {:<55} {:>9.1} ns  {:.3} allocs  {:.3} reallocs  {:.4} faults  {:.4} machinery",
                all[at].0, cost.ns, cost.allocs, cost.reallocs, cost.faults, cost.machinery
            );
            costs[at].push(cost);
        }
    }
    let (least, most) = loads
        .iter()
        .fold((f64::MAX, f64::MIN), |(l, m), x| (l.min(*x), m.max(*x)));
    println!();
    println!("{rounds} rounds, load {least:.1}-{most:.1}; per step, median (least-most)");
    println!(
        "| workload | ns a step | allocations | reallocations | page faults | the machinery's allocations and reallocations |"
    );
    println!("|---|---|---|---|---|---|");
    for (at, (name, _)) in all.iter().enumerate() {
        let mut ns: Vec<f64> = costs[at].iter().map(|c| c.ns).collect();
        ns.sort_by(f64::total_cmp);
        let median = |v: &[f64]| v[v.len() / 2];
        let mut allocs: Vec<f64> = costs[at].iter().map(|c| c.allocs).collect();
        allocs.sort_by(f64::total_cmp);
        let mut reallocs: Vec<f64> = costs[at].iter().map(|c| c.reallocs).collect();
        reallocs.sort_by(f64::total_cmp);
        let mut faults: Vec<f64> = costs[at].iter().map(|c| c.faults).collect();
        faults.sort_by(f64::total_cmp);
        let mut machinery: Vec<f64> = costs[at].iter().map(|c| c.machinery).collect();
        machinery.sort_by(f64::total_cmp);
        println!(
            "| {name} | {:.1} ({:.1}-{:.1}) | {:.3} | {:.3} | {:.4} | {:.4} |",
            median(&ns),
            ns[0],
            ns[ns.len() - 1],
            median(&allocs),
            median(&reallocs),
            median(&faults),
            median(&machinery)
        );
    }
}
