//! hyper-swim's own failure detector, driven over a simulated LAN (S8's crash class; F1 in
//! `docs/design/failure-isolation.md`, `hex-sim.md` H40): the membership the design gives the node
//! agent (which runs none yet), vendored unchanged from hyper-raft (`vendor/MANIFEST.toml`), not a
//! model of it. Each member is a `hyper_swim` `Detector`; this module is only its owner: it
//! delivers the pings, acknowledgements, ping-requests, relayed answers and nacks the detectors ask
//! for after a seeded one-way delay, carries their gossip, wakes each when it asks, and crashes one
//! member after a warm-up. One seed, virtual nanoseconds, a bounded event queue (H1, H5); handlers
//! never call one another in a cycle (CLAUDE.md §11).
//!
//! What it measures of the crash: when a live member's own probes first suspect the crashed one,
//! when one first condemns it, and, for every condemnation, the time from the victim's last answer
//! against the bound the condemner stated (`PeerReport::condemned_after` and `condemned_within`:
//! `Detector::detection_bound` plus the wait it measured for another member's answer).

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::num::NonZeroUsize;
use std::time::Duration;

use hyper_swim::HostId;
use hyper_swim::codec::{Coordinate, GossipBatch, SwimMessage, gossip_capacity};
use hyper_swim::detector::{Detector, Finding, PingReq};
use hyper_swim::membership::MemberState;
use hyper_timing::Exposure;

use crate::SimError;
use crate::model::Time;
use crate::rng::Rng;

/// The path's datagram size: QUIC's minimum, which every path carries (RFC 9000 §14.1), as
/// hyper-swim's own owner sizes its messages (hyper-raft `crates/hyper-swim/tests/sim.rs:48-49`).
pub const DATAGRAM: usize = 1_200;

/// The gossip entries one message carries: what the datagram leaves beside the largest message
/// hyper-swim sends with an empty batch, an acknowledgement with this member's coordinate and every
/// field at its widest (`codec::gossip_capacity`).
pub fn gossip_room(detector: &Detector) -> usize {
    let coordinate = *detector.coordinate();
    let ack = SwimMessage::Ack {
        from: HostId(u64::MAX),
        nonce: u64::MAX,
        boot_nonce: u64::MAX,
        configuration_version: u64::MAX,
        standing: Some(u64::MAX),
        promise_ns: u64::MAX,
        relay_within_ns: u64::MAX,
        gossip: GossipBatch::Entries(&[]),
        coordinate: Coordinate::Held(&coordinate),
    };
    let mut bare = Vec::new();
    ack.encode_into(&mut bare);
    gossip_capacity(DATAGRAM, bare.len())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub members: u32,
    /// The owner's detection budget (`Detector::new`): a judged period lasts at least its share of one
    /// period, `budget / (2(2m − 1) + 1)`. Zero leaves each period at what its probe needs.
    pub budget_ns: Time,
    pub net_min_ns: Time,
    pub net_max_ns: Time,
    /// The member crashed and when.
    pub victim: u32,
    pub crash_ns: Time,
    /// The run ends at this virtual time, or once a live member condemned the victim.
    pub end_ns: Time,
    pub max_events: usize,
    pub max_steps: u64,
    /// Probes one member relays at once.
    pub max_relayed: usize,
}

impl Config {
    /// A LAN of `members`, one-way delays of 100 µs to 2 ms as the memberlist model's
    /// ([`crate::membership::Config::lan`]), under detection budget `budget`.
    pub fn lan(members: u32, budget: Duration) -> Self {
        Self {
            members,
            budget_ns: u64::try_from(budget.as_nanos()).unwrap_or(u64::MAX),
            net_min_ns: 100_000,
            net_max_ns: 2_000_000,
            victim: members.saturating_sub(1),
            crash_ns: 60_000_000_000,
            end_ns: 3_600_000_000_000,
            max_events: 1 << 22,
            max_steps: 1 << 32,
            max_relayed: 64,
        }
    }
}

/// One condemnation of the victim: from its last answer to the condemner, and the bound it stated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Condemnation {
    pub by: u32,
    pub at_ns: Time,
    pub after: Duration,
    pub within: Option<Duration>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The first suspicion of the victim by a live member's own probe, virtual time.
    pub first_suspected_ns: Option<Time>,
    /// The first condemnation of it, with its evidence.
    pub first_condemned: Option<Condemnation>,
    /// Condemnations of members that never crashed.
    pub false_condemnations: u64,
    /// Pings the detectors sent, their own probes and relayed ones.
    pub pings: u64,
    /// The largest detection bound a live member stated at the crash.
    pub bound_at_crash: Option<Duration>,
    pub events: u64,
}

#[derive(Debug, Clone)]
enum Msg {
    Ping {
        nonce: u64,
        gossip: Vec<(HostId, MemberState)>,
    },
    Ack {
        nonce: u64,
        relay_within_ns: u64,
        gossip: Vec<(HostId, MemberState)>,
    },
    PingReq {
        target: HostId,
        nonce: u64,
    },
    IndirectAck {
        target: HostId,
        nonce: u64,
    },
    Nack {
        target: HostId,
        nonce: u64,
    },
}

#[derive(Debug, Clone)]
enum Event {
    Wake { member: u32 },
    Deliver { from: u32, to: u32, msg: Msg },
}

/// A probe this member relays for another: the nonce it pinged the target with, and its asker's.
#[derive(Debug, Clone, Copy)]
struct Relaying {
    relayed: u64,
    asker: HostId,
    asked: u64,
    due_ns: Time,
    nacked: bool,
}

struct Member {
    detector: Detector,
    alive: bool,
    /// Keyed by (asker, target).
    relaying: BTreeMap<(u64, u64), Relaying>,
    /// The wake already queued, so a member isn't queued twice for one instant.
    queued_wake: Option<Time>,
}

struct World {
    config: Config,
    now: Time,
    seq: u64,
    queue: BinaryHeap<Reverse<(Time, u64, usize)>>,
    /// The queued events, by the slot the heap names; a freed slot is reused.
    events: Vec<Option<Event>>,
    free: Vec<usize>,
    members: Vec<Member>,
    rng: Rng,
    report: Report,
    requests: Vec<PingReq>,
    batch: Vec<(HostId, MemberState)>,
    /// Gossip entries a message carries ([`gossip_room`]).
    gossip_room: usize,
}

fn host(member: u32) -> HostId {
    HostId(u64::from(member))
}

fn member_of(host: HostId) -> Result<u32, SimError> {
    u32::try_from(host.0).map_err(|_| SimError::Config("a host id past the members"))
}

fn index(member: u32) -> Result<usize, SimError> {
    usize::try_from(member).map_err(|_| SimError::Config("a member index"))
}

impl World {
    fn member(&self, m: u32) -> Result<&Member, SimError> {
        self.members
            .get(index(m)?)
            .ok_or(SimError::Config("a member past the members"))
    }

    fn member_mut(&mut self, m: u32) -> Result<&mut Member, SimError> {
        self.members
            .get_mut(index(m)?)
            .ok_or(SimError::Config("a member past the members"))
    }

    fn push(&mut self, at: Time, event: Event) -> Result<(), SimError> {
        let slot = match self.free.pop() {
            Some(slot) => {
                if let Some(held) = self.events.get_mut(slot) {
                    *held = Some(event);
                }
                slot
            }
            None => {
                if self.events.len() >= self.config.max_events {
                    return Err(SimError::Full {
                        what: "the event queue",
                        bound: self.config.max_events,
                    });
                }
                self.events.push(Some(event));
                self.events.len().saturating_sub(1)
            }
        };
        self.seq = self.seq.checked_add(1).ok_or(SimError::TimeOverflow)?;
        self.queue.push(Reverse((at, self.seq, slot)));
        Ok(())
    }

    fn send(&mut self, from: u32, to: HostId, msg: Msg) -> Result<(), SimError> {
        let delay = self
            .rng
            .range(self.config.net_min_ns, self.config.net_max_ns);
        let at = self.now.checked_add(delay).ok_or(SimError::TimeOverflow)?;
        let to = member_of(to)?;
        self.push(at, Event::Deliver { from, to, msg })
    }

    /// Queue `m`'s next wake: its detector's, or a relayed probe's deadline, whichever is first.
    fn schedule(&mut self, m: u32) -> Result<(), SimError> {
        let member = self.member(m)?;
        let relayed = member
            .relaying
            .values()
            .filter(|r| !r.nacked)
            .map(|r| r.due_ns)
            .min();
        let next = match (member.detector.wake(), relayed) {
            (Some(own), Some(r)) => Some(own.min(r)),
            (own, r) => own.or(r),
        };
        let Some(at) = next.map(|at| at.max(self.now)) else {
            return Ok(());
        };
        if member.queued_wake.is_some_and(|q| q <= at && q >= self.now) {
            return Ok(());
        }
        self.member_mut(m)?.queued_wake = Some(at);
        self.push(at, Event::Wake { member: m })
    }

    /// `m` wakes: nack every relayed probe past its deadline, poll its detector, send what it asks,
    /// and note what its own probes found.
    fn wake(&mut self, m: u32) -> Result<(), SimError> {
        let now = self.now;
        let expired: Vec<((u64, u64), Relaying)> = self
            .member(m)?
            .relaying
            .iter()
            .filter(|(_, r)| !r.nacked && r.due_ns <= now)
            .map(|(k, r)| (*k, *r))
            .collect();
        for (key, r) in expired {
            if let Some(held) = self.member_mut(m)?.relaying.get_mut(&key) {
                held.nacked = true;
            }
            let target = HostId(key.1);
            self.send(
                m,
                r.asker,
                Msg::Nack {
                    target,
                    nonce: r.asked,
                },
            )?;
        }
        let mut requests = std::mem::take(&mut self.requests);
        let ping = self.member_mut(m)?.detector.poll(now, &mut requests);
        for r in &requests {
            self.send(
                m,
                r.relay,
                Msg::PingReq {
                    target: r.target,
                    nonce: r.nonce,
                },
            )?;
        }
        self.requests = requests;
        if let Some(ping) = ping {
            let room = self.gossip_room;
            let mut batch = std::mem::take(&mut self.batch);
            batch.clear();
            self.member_mut(m)?
                .detector
                .ping_gossip_into(ping.to, room, &mut batch);
            self.report.pings = self.report.pings.saturating_add(1);
            let gossip = batch.clone();
            self.batch = batch;
            self.send(
                m,
                ping.to,
                Msg::Ping {
                    nonce: ping.nonce,
                    gossip,
                },
            )?;
        }
        self.note_findings(m)?;
        self.schedule(m)
    }

    fn note_findings(&mut self, m: u32) -> Result<(), SimError> {
        let victim = host(self.config.victim);
        let crashed = self.now >= self.config.crash_ns;
        let findings: Vec<Finding> = self.member(m)?.detector.findings().to_vec();
        for f in findings {
            match f {
                Finding::Suspected(u) if u.target == victim && crashed => {
                    self.report.first_suspected_ns.get_or_insert(self.now);
                }
                Finding::Condemned { target, at_ns, .. } => {
                    if target != victim || !crashed {
                        self.report.false_condemnations =
                            self.report.false_condemnations.saturating_add(1);
                        continue;
                    }
                    if self.report.first_condemned.is_none() {
                        let detector = &self.member(m)?.detector;
                        let report = detector.report(target).unwrap_or_default();
                        self.report.first_condemned = Some(Condemnation {
                            by: m,
                            at_ns,
                            after: report.condemned_after.unwrap_or_default(),
                            within: report.condemned_within,
                        });
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn deliver(&mut self, from: u32, to: u32, msg: Msg) -> Result<(), SimError> {
        let now = self.now;
        let from_host = host(from);
        match msg {
            Msg::Ping { nonce, gossip } => {
                let member = self.member_mut(to)?;
                member.detector.apply_gossip(gossip);
                let ack = member.detector.on_ping(from_host);
                let room = self.gossip_room;
                let mut batch = std::mem::take(&mut self.batch);
                batch.clear();
                self.member_mut(to)?
                    .detector
                    .ack_gossip_into(from_host, room, &mut batch);
                let gossip = batch.clone();
                self.batch = batch;
                self.send(
                    to,
                    ack.to,
                    Msg::Ack {
                        nonce,
                        relay_within_ns: ack.relay_within_ns,
                        gossip,
                    },
                )?;
            }
            Msg::Ack {
                nonce,
                relay_within_ns,
                gossip,
            } => {
                let member = self.member_mut(to)?;
                member.detector.apply_gossip(gossip);
                let relayed = member
                    .relaying
                    .iter()
                    .find(|((_, target), r)| *target == from_host.0 && r.relayed == nonce)
                    .map(|(k, r)| (*k, *r));
                match relayed {
                    // The target's answer goes back to the asker, after a nack too.
                    Some((key, r)) => {
                        member.relaying.remove(&key);
                        self.send(
                            to,
                            r.asker,
                            Msg::IndirectAck {
                                target: from_host,
                                nonce: r.asked,
                            },
                        )?;
                    }
                    None => member
                        .detector
                        .on_ack(from_host, nonce, now, relay_within_ns),
                }
            }
            Msg::PingReq { target, nonce } => {
                let max = self.config.max_relayed;
                let member = self.member_mut(to)?;
                if member.relaying.len() >= max {
                    return Err(SimError::Full {
                        what: "a member's relayed probes",
                        bound: max,
                    });
                }
                let ping = member.detector.on_ping_req(from_host, target, now);
                member.relaying.insert(
                    (from_host.0, target.0),
                    Relaying {
                        relayed: ping.nonce,
                        asker: from_host,
                        asked: nonce,
                        due_ns: ping.due_ns,
                        nacked: false,
                    },
                );
                self.report.pings = self.report.pings.saturating_add(1);
                self.send(
                    to,
                    target,
                    Msg::Ping {
                        nonce: ping.nonce,
                        gossip: Vec::new(),
                    },
                )?;
            }
            Msg::IndirectAck { target, nonce } => {
                self.member_mut(to)?
                    .detector
                    .on_indirect_ack(target, nonce, now);
            }
            Msg::Nack { target, nonce } => {
                self.member_mut(to)?
                    .detector
                    .on_indirect_nack(from_host, target, nonce, now);
            }
        }
        // A message may end a period or move a deadline: poll at once, as the owner must.
        self.wake(to)
    }
}

/// One run: `config.members` detectors, every member joined to every other, the victim crashed at
/// `config.crash_ns`; until a live member condemns it or the run's end.
pub fn run(config: &Config, seed: u64) -> Result<Report, SimError> {
    let members =
        NonZeroUsize::new(index(config.members)?).ok_or(SimError::Config("a run of no members"))?;
    if config.members < 2
        || config.victim >= config.members
        || config.net_min_ns > config.net_max_ns
    {
        return Err(SimError::Config("members, victim or network delays"));
    }
    let budget = Duration::from_nanos(config.budget_ns);
    let mut all = Vec::with_capacity(members.get());
    for m in 0..config.members {
        // The simulation's clock reads whole nanoseconds.
        let mut detector = Detector::new(
            host(m),
            Exposure::new(),
            members,
            Duration::from_nanos(1),
            budget,
        );
        for peer in (0..config.members).filter(|p| *p != m) {
            detector
                .join(host(peer))
                .map_err(|_| SimError::Config("a view too small for its members"))?;
        }
        all.push(Member {
            detector,
            alive: true,
            relaying: BTreeMap::new(),
            queued_wake: None,
        });
    }
    let mut w = World {
        config: *config,
        now: 0,
        seq: 0,
        queue: BinaryHeap::new(),
        events: Vec::new(),
        free: Vec::new(),
        members: all,
        rng: Rng::new(seed),
        report: Report::default(),
        requests: Vec::new(),
        batch: Vec::new(),
        gossip_room: 0,
    };
    w.gossip_room = w
        .members
        .first()
        .map_or(0, |member| gossip_room(&member.detector));
    // Members start a seeded fraction of a millisecond apart, so their periods don't align.
    for m in 0..config.members {
        let at = w.rng.range(0, 1_000_000);
        w.member_mut(m)?.queued_wake = Some(at);
        w.push(at, Event::Wake { member: m })?;
    }
    let mut crashed = false;
    while let Some(Reverse((at, _, slot))) = w.queue.pop() {
        if at > config.end_ns || w.report.first_condemned.is_some() {
            break;
        }
        if !crashed && at >= config.crash_ns {
            crashed = true;
            w.now = config.crash_ns;
            w.report.bound_at_crash = (0..config.members)
                .filter(|m| *m != config.victim)
                .filter_map(|m| w.member(m).ok())
                .filter_map(|member| member.detector.detection_bound(config.crash_ns))
                .max();
            w.member_mut(config.victim)?.alive = false;
        }
        w.now = at;
        w.report.events = w.report.events.saturating_add(1);
        if w.report.events > config.max_steps {
            return Err(SimError::StepBudget(config.max_steps));
        }
        let event = w
            .events
            .get_mut(slot)
            .and_then(Option::take)
            .ok_or(SimError::Config("a queued event's slot"))?;
        w.free.push(slot);
        match event {
            Event::Wake { member } => {
                let m = w.member_mut(member)?;
                if !m.alive || m.queued_wake != Some(at) {
                    continue;
                }
                m.queued_wake = None;
                w.wake(member)?;
            }
            Event::Deliver { from, to, msg } => {
                if !w.member(to)?.alive {
                    continue;
                }
                w.deliver(from, to, msg)?;
            }
        }
    }
    Ok(w.report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_swim::membership::Liveness;

    /// S8 crash class (F1), gossip sized from the datagram, not picked: do fill an acknowledgement at
    /// its widest with `gossip_room` entries; expect it to fit QUIC's 1,200-byte datagram, one more
    /// entry not to, and the room to carry some gossip.
    #[test]
    fn the_gossip_room_is_what_the_datagram_leaves() {
        let d = Detector::new(
            HostId(0),
            Exposure::new(),
            NonZeroUsize::new(128).unwrap(),
            Duration::from_nanos(1),
            Duration::ZERO,
        );
        let room = gossip_room(&d);
        assert!(room > 0);
        let state = MemberState {
            liveness: Liveness::Suspect,
            incarnation: u64::MAX,
        };
        let coordinate = *d.coordinate();
        let encoded = |entries: usize| {
            let batch = vec![(HostId(u64::MAX), state); entries];
            let ack = SwimMessage::Ack {
                from: HostId(u64::MAX),
                nonce: u64::MAX,
                boot_nonce: u64::MAX,
                configuration_version: u64::MAX,
                standing: Some(u64::MAX),
                promise_ns: u64::MAX,
                relay_within_ns: u64::MAX,
                gossip: GossipBatch::Entries(&batch),
                coordinate: Coordinate::Held(&coordinate),
            };
            let mut out = Vec::new();
            ack.encode_into(&mut out);
            out.len()
        };
        assert!(encoded(room) <= DATAGRAM, "{} > {DATAGRAM}", encoded(room));
        assert!(encoded(room + 1) > DATAGRAM, "{}", encoded(room + 1));
    }
}
