//! CP-T3.2. The session group's production membership adapter, real replicas and the node
//! detectors on one virtual clock. The seeded network schedules both protocols independently.
use anyhow::{Context, Result, ensure};
use hyper_durable::{Bytes, EntryRef, Fatal, Output, Point, RamStore, Replica, StateMachine};
use hyper_liveness::{Change, Write};
use hyper_raft::proto::{ConfChangeV2, ConfState, Message};
use hyper_raft::wire::Record as _;
use kiln_sim::{Queue, Rng};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::task::Waker;
use std::time::Duration;

use crate::convo::liveness::Membership;
use crate::liveness::Shared;

#[derive(Clone, Default)]
struct Ledger {
    point: Point,
    conf: ConfState,
    rows: Vec<Vec<u8>>,
}
impl StateMachine for Ledger {
    type Answer = ();
    fn apply(&mut self, entry: &EntryRef<'_>, _: &mut Vec<()>) -> Result<(), Fatal> {
        self.point = Point {
            index: entry.index,
            term: entry.term,
        };
        if !entry.data.is_empty() {
            self.rows.push(entry.data.to_vec());
        }
        Ok(())
    }
    fn apply_change(&mut self, at: Point, _: &ConfChangeV2, conf: &ConfState) -> Result<(), Fatal> {
        self.point = at;
        self.conf.clone_from(conf);
        Ok(())
    }
    fn durable(&self) -> Point {
        self.point
    }
    fn configuration(&self) -> &ConfState {
        &self.conf
    }
    fn acts_at_start(&self, _: &EntryRef<'_>) -> bool {
        true
    }
    fn image(&mut self, dest: &mut Vec<u8>) -> Result<(Point, ConfState), Fatal> {
        serde_json::to_writer(dest, &self.rows).map_err(|_| Fatal("image"))?;
        Ok((self.point, self.conf.clone()))
    }
    fn image_bytes(&self) -> Option<u64> {
        None
    }
    fn install(&mut self, bytes: &[u8], at: Point, conf: &ConfState) -> Result<(), Fatal> {
        self.rows = serde_json::from_slice(bytes).map_err(|_| Fatal("image"))?;
        self.point = at;
        self.conf.clone_from(conf);
        Ok(())
    }
    fn persist(&mut self) -> Result<(), Fatal> {
        Ok(())
    }
}
type Member = Replica<RamStore, Ledger, Bytes>;

#[derive(Default)]
pub(crate) struct Beats {
    pub messages: Vec<(u64, Vec<u8>)>,
    pub changes: Vec<Change>,
    pub flush: bool,
}
impl hyper_liveness::Output for Beats {
    fn heartbeat(&mut self, peer: u64, message: &[u8]) {
        self.messages.push((peer, message.to_vec()));
    }
    fn change(&mut self, change: Change) {
        self.changes.push(change);
    }
    fn flush(&mut self) {
        self.flush = true;
    }
}

enum Packet {
    Beat(Vec<u8>),
    Raft(Message),
}
struct Node {
    shared: Arc<Shared>,
    group: Member,
    membership: Membership,
    flush: Option<u64>,
    alarm: Option<u64>,
    alive: bool,
    stalled: Option<u64>,
    slow: bool,
}
struct Fleet {
    nodes: BTreeMap<u64, Node>,
    wire: Queue<(u64, u64, Packet)>,
    cut: BTreeSet<(u64, u64)>,
    now: u64,
    rng: Rng,
    scale: u64,
    raft: u64,
    heartbeat: BTreeMap<u64, u64>,
    elected: BTreeMap<u64, u64>,
    suspicions: Vec<(u64, hyper_liveness::Suspicion)>,
    sessions: Vec<Session>,
}
const LINK: u64 = 200_000;
const FLUSH: u64 = 2_000_000;
fn ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl Fleet {
    fn new(voters: &[u64]) -> Result<Self> {
        Self::scaled(voters, 1)
    }
    fn scaled(voters: &[u64], scale: u64) -> Result<Self> {
        let mut nodes = BTreeMap::new();
        for &id in voters {
            let shared = Shared::isolated(id);
            shared.initialize_for_test(hyper_liveness::Settings {
                local: id,
                run: 1,
                max_peers: voters.len(),
                resolution: Duration::from_nanos(scale),
                history: hyper_timing::Exposure::new(),
            })?;
            let settings = crate::convo::raft::raft_settings(id, 1)?;
            let ledger = Ledger {
                conf: ConfState {
                    voters: voters.to_vec(),
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut group = Replica::open(
                &settings,
                RamStore::new(),
                ledger,
                Bytes::new(settings.core.max_size_per_msg.saturating_mul(4)),
            )?;
            let mut membership = Membership::new(shared.clone(), 1, None, true);
            membership.reconcile(&mut group, true)?;
            nodes.insert(
                id,
                Node {
                    shared,
                    group,
                    membership,
                    flush: None,
                    alarm: Some(0),
                    alive: true,
                    stalled: None,
                    slow: false,
                },
            );
        }
        let mut fleet = Self {
            nodes,
            wire: Queue::new(),
            cut: BTreeSet::new(),
            now: 0,
            rng: Rng::new(13),
            scale,
            raft: 0,
            heartbeat: BTreeMap::new(),
            elected: BTreeMap::new(),
            suspicions: Vec::new(),
            sessions: Vec::new(),
        };
        if voters.len() == 1 {
            let id = *voters.first().context("no voter")?;
            fleet.node(id)?.group.campaign()?;
        }
        Ok(fleet)
    }
    fn node(&mut self, id: u64) -> Result<&mut Node> {
        self.nodes.get_mut(&id).context("unknown node")
    }
    fn leader(&self) -> Option<u64> {
        self.nodes
            .iter()
            .find_map(|(id, n)| (n.alive && n.group.is_leader()).then_some(*id))
    }
    fn send(&mut self, from: u64, to: u64, packet: Packet) -> Result<()> {
        match &packet {
            Packet::Raft(m) => self.raft += m.encoded_len() as u64,
            Packet::Beat(b) => {
                let payload = crate::reach::heartbeat::ENVELOPE_BYTES + b.len();
                *self.heartbeat.entry(from).or_default() += u64::try_from(payload)?;
            }
        }
        if !self.cut.contains(&(from, to)) && self.nodes.get(&to).is_some_and(|n| n.alive) {
            let at =
                Duration::from_nanos(self.now + (LINK + self.rng.below(LINK / 8)) * self.scale);
            ensure!(self.wire.push(at, (from, to, packet)), "virtual link full");
        }
        Ok(())
    }
    fn act(&mut self, id: u64, received: Option<(u64, Packet)>) -> Result<()> {
        let now = self.now;
        let flush = FLUSH * self.scale;
        let mut beats = Beats::default();
        let mut messages = Vec::new();
        let mut suspicions = Vec::new();
        let mut timing_changed = false;
        let mut verdicts = Vec::new();
        let leader_term = {
            let node = self.node(id)?;
            if !node.alive {
                return Ok(());
            }
            if node.stalled.is_none()
                && node.flush.is_some_and(|start| now >= start + flush)
                && let Some(start) = node.flush.take()
            {
                node.shared
                    .with(|l| l.on_durable(Write::Liveness, start, now));
            }
            if let Some((from, packet)) = received {
                match packet {
                    Packet::Beat(bytes) => {
                        timing_changed = node.shared.with(|l| {
                            let before = l.report(from).map(|r| r.configurations);
                            let measurable = l.round_trip(from).is_some_and(|r| r.samples() > 0);
                            let ok = l.on_heartbeat(from, &bytes, now, &mut beats).is_ok();
                            ok && (before != l.report(from).map(|r| r.configurations)
                                || !measurable
                                    && l.round_trip(from).is_some_and(|r| r.samples() > 0))
                        }) == Some(true);
                    }
                    Packet::Raft(message) => node.group.step(message)?,
                }
            }
            node.shared.with(|l| {
                if let Some(at) = node.stalled {
                    l.on_pending(Some(at));
                }
                if let Some(due) = l.wake()
                    && now >= due
                {
                    l.on_wait(due, now);
                }
                l.poll(now, &mut beats);
            });
            if beats.flush && node.flush.is_none() {
                node.flush = Some(now);
            }
            for change in beats.changes.drain(..) {
                verdicts.push(change);
                node.membership.belief(&mut node.group, change)?;
                if let Change::Suspected(s) = change {
                    suspicions.push((id, s));
                }
            }
            let slow = node.shared.with(|l| l.own_disk_slow()) == Some(true);
            if slow && !node.slow {
                node.membership
                    .yield_stalled_log(&mut node.group, |_| true)?;
            }
            node.slow = slow;
            if timing_changed || !node.membership.timed() {
                node.membership.measure(&mut node.group)?;
            }
            let mut out = Output::default();
            loop {
                out.clear();
                let driven = node.group.drive(now, Waker::noop(), &mut out)?;
                node.membership.io(&driven);
                messages.append(&mut out.messages);
                if !driven.more && driven.out == 0 {
                    break;
                }
            }
            node.membership.reconcile(&mut node.group, false)?;
            node.alarm = [
                node.shared.with(|l| l.wake()).flatten(),
                node.group.deadline(),
                node.flush
                    .filter(|_| node.stalled.is_none())
                    .map(|at| at + flush),
            ]
            .into_iter()
            .flatten()
            .filter(|at| *at > now)
            .min();
            node.group.is_leader().then(|| node.group.term())
        };
        if timing_changed || !verdicts.is_empty() {
            for session in &mut self.sessions {
                if let Some((r, m)) = session.0.iter_mut().find(|(r, _)| r.id() == id) {
                    for &change in &verdicts {
                        m.belief(r, change)?;
                    }
                    if timing_changed {
                        m.measure(r)?;
                    }
                }
                if !verdicts.is_empty() {
                    self.raft += session.drive(now)?;
                }
            }
        }
        if let Some(term) = leader_term {
            ensure!(
                self.elected
                    .insert(term, id)
                    .is_none_or(|other| other == id),
                "term {term} had two leaders"
            );
        }
        self.suspicions.extend(suspicions);
        for m in messages {
            self.send(id, m.to, Packet::Raft(m))?;
        }
        for (to, bytes) in beats.messages {
            self.send(id, to, Packet::Beat(bytes))?;
        }
        Ok(())
    }
    fn next(&mut self) -> Result<()> {
        let alarm = self
            .nodes
            .iter()
            .filter(|(_, n)| n.alive)
            .filter_map(|(id, n)| n.alarm.map(|at| (at, *id)))
            .min();
        let wire = self.wire.next_at().map(ns);
        if wire.is_some_and(|t| alarm.is_none_or(|(a, _)| t <= a)) {
            let (at, (from, to, packet)) = self.wire.pop().context("wire empty")?;
            self.now = self.now.max(ns(at));
            if !self.cut.contains(&(from, to)) {
                self.act(to, Some((from, packet)))?;
            }
        } else if let Some((at, id)) = alarm {
            self.now = self.now.max(at) + self.rng.below(20_000) * self.scale;
            self.act(id, None)?;
        } else {
            anyhow::bail!("no scheduled work");
        }
        Ok(())
    }
    fn until(&mut self, predicate: impl Fn(&Self) -> bool) -> Result<()> {
        let expires = self.now + ns(Duration::from_secs(90)) * self.scale;
        while !predicate(self) {
            ensure!(
                self.now < expires,
                "condition unmet at {} with leader {:?} and terms {:?}",
                self.now,
                self.leader(),
                self.elected
            );
            self.next()?;
        }
        Ok(())
    }
    fn settled(&mut self) -> Result<u64> {
        self.until(|w| {
            w.leader().is_some()
                && w.nodes.iter().all(|(&id, n)| {
                    w.nodes.keys().all(|&other| {
                        id == other
                            || n.shared.with(|l| {
                                l.report(other).is_some_and(|r| r.judged)
                                    && matches!(
                                        l.trust(other),
                                        Some(hyper_timing::Trust::Trusted { .. })
                                    )
                            }) == Some(true)
                    })
                })
        })?;
        self.leader().context("no leader")
    }
    fn propose(&mut self, id: u64, rows: &[u8]) -> Result<()> {
        self.node(id)?.group.propose(Vec::new(), rows.to_vec())?;
        self.act(id, None)
    }
    fn cut(&mut self, a: u64, b: u64) {
        self.cut.insert((a, b));
        self.cut.insert((b, a));
    }
}

#[test]
fn a_session_survives_its_leaders_kill() -> Result<()> {
    let mut w = Fleet::new(&[1, 2, 3])?;
    let old = w.settled()?;
    w.propose(old, b"acknowledged")?;
    w.until(|w| {
        w.nodes
            .values()
            .all(|n| n.group.machine().rows.iter().any(|r| r == b"acknowledged"))
    })?;
    let old = w.settled()?;
    let first = w.now;
    let terms = w.elected.len();
    w.node(old)?.alive = false;
    w.until(|w| w.leader().is_some_and(|leader| leader != old))?;
    let new = w.leader().context("no successor")?;
    let elected = w.now;
    let (_, s) = w
        .suspicions
        .iter()
        .find(|(id, s)| *id == new && s.peer == old && s.at_ns >= first)
        .context("no observed failure")?;
    let due = s.last.context("failure lacked last heartbeat")?.due_ns;
    let bound = ns(s.detection.context("no detection bound")?);
    let span = ns(w
        .nodes
        .get(&new)
        .and_then(|n| n.membership.election())
        .context("no measured election")?);
    ensure!(
        s.at_ns <= due.saturating_add(bound)
            && elected <= first.saturating_add(bound).saturating_add(span),
        "elected at {elected}, promised detection {bound} from {due}, measured election {span}"
    );
    ensure!(w.elected.len() == terms + 1);
    w.propose(new, b"successor")?;
    w.until(|w| {
        w.nodes
            .values()
            .filter(|n| n.alive)
            .all(|n| n.group.machine().rows == [b"acknowledged".to_vec(), b"successor".to_vec()])
    })?;
    Ok(())
}

#[test]
fn one_link_dropping_does_not_move_the_lead() -> Result<()> {
    let mut w = Fleet::new(&[1, 2, 3])?;
    let leader = w.settled()?;
    let other = *w
        .nodes
        .keys()
        .find(|id| **id != leader)
        .context("no peer")?;
    let healthy = *w
        .nodes
        .keys()
        .find(|&&id| id != leader && id != other)
        .context("no independent path")?;
    let term = w.node(leader)?.group.term();
    let began = w.suspicions.len();
    let sees = |fleet: &Fleet, voter, missing| {
        fleet.nodes.get(&voter).is_some_and(|n| {
            n.shared.with(|l| {
                if missing {
                    l.trust(leader) == Some(hyper_timing::Trust::Suspected)
                } else {
                    matches!(l.trust(leader), Some(hyper_timing::Trust::Trusted { .. }))
                }
            }) == Some(true)
        })
    };
    for _ in 0..3 {
        w.cut(leader, other);
        w.until(|w| sees(w, other, true))?;
        let election = w
            .node(other)?
            .membership
            .election()
            .context("missing measured election")?;
        let until = w.now + ns(election);
        w.until(|w| w.now >= until)?;
        ensure!(
            sees(&w, healthy, false)
                && w.leader() == Some(leader)
                && w.node(leader)?.group.term() == term
        );
        w.cut.remove(&(leader, other));
        w.cut.remove(&(other, leader));
        w.until(|w| sees(w, other, false))?;
        ensure!(
            sees(&w, healthy, false)
                && w.leader() == Some(leader)
                && w.node(leader)?.group.term() == term
        );
    }
    ensure!(
        !w.suspicions
            .iter()
            .skip(began)
            .any(|(id, s)| *id == healthy && s.peer == leader)
    );
    Ok(())
}

#[test]
fn a_partitioned_minority_leader_steps_down() -> Result<()> {
    let mut w = Fleet::new(&[1, 2, 3])?;
    let old = w.settled()?;
    for other in [1, 2, 3] {
        if other != old {
            w.cut(old, other);
        }
    }
    w.until(|w| {
        w.nodes.get(&old).is_some_and(|n| !n.group.is_leader())
            && w.leader().is_some_and(|lead| lead != old)
    })?;
    Ok(())
}
#[test]
fn one_voter_never_elects_anyone_else() -> Result<()> {
    let mut w = Fleet::new(&[1])?;
    w.act(1, None)?;
    ensure!(w.leader() == Some(1) && w.nodes.values().all(|n| n.group.deadline().is_none()));
    ensure!(w.raft == 0 && w.heartbeat.is_empty());
    Ok(())
}
#[test]
fn two_voters_one_down_no_election() -> Result<()> {
    let mut w = Fleet::new(&[1, 2])?;
    let old = w.settled()?;
    w.node(old)?.alive = false;
    w.until(|w| w.suspicions.iter().any(|(_, s)| s.peer == old))?;
    let until = w.now + ns(Duration::from_millis(200));
    w.until(|w| w.now >= until)?;
    ensure!(w.leader().is_none());
    Ok(())
}
#[test]
fn two_candidates_at_once_one_leader_per_term() -> Result<()> {
    let mut w = Fleet::new(&[1, 2, 3])?;
    let old = w.settled()?;
    w.node(old)?.alive = false;
    let others: Vec<_> = w.nodes.keys().copied().filter(|id| *id != old).collect();
    for id in others {
        w.node(id)?.group.campaign()?;
        w.act(id, None)?;
    }
    w.until(|w| w.leader().is_some())?;
    // Every act records term and identity and checks any previous leader in that term.
    ensure!(w.elected.len() >= 2);
    Ok(())
}

#[test]
fn a_leader_whose_disk_stalls_loses_the_lead_but_its_agent_is_not_moved() -> Result<()> {
    let mut w = Fleet::new(&[1, 2, 3])?;
    let owner = w.settled()?;
    w.propose(owner, b"committed provider epoch 1")?;
    w.until(|w| w.nodes.values().all(|n| !n.group.machine().rows.is_empty()))?;
    let now = w.now;
    w.node(owner)?.stalled = Some(now);
    w.until(|w| {
        w.leader().is_some_and(|l| l != owner) && w.nodes.get(&owner).is_some_and(|n| n.slow)
    })?;
    ensure!(
        w.nodes
            .iter()
            .filter(|(id, _)| **id != owner)
            .all(|(_, n)| !matches!(
                n.shared.with(|l| l.trust(owner)),
                Some(Some(hyper_timing::Trust::Suspected))
            ) && n.group.machine().rows == [b"committed provider epoch 1".to_vec()])
    );
    Ok(())
}

#[test]
fn a_verdict_during_a_membership_change() -> Result<()> {
    let mut w = Fleet::new(&[1, 2, 3])?;
    let leader = w.settled()?;
    let node = w.node(leader)?;
    let old = node.group.configuration().clone();
    let other = *old
        .voters
        .iter()
        .find(|id| **id != leader)
        .context("no other member")?;
    let add = ConfChangeV2 {
        changes: vec![hyper_raft::proto::ConfChangeSingle {
            node_id: 4,
            change_type: hyper_raft::proto::ConfChangeType::AddLearnerNode,
        }],
        ..Default::default()
    };
    node.group.change(Vec::new(), &add)?;
    node.membership.belief(
        &mut node.group,
        Change::DiskSlow {
            peer: other,
            at_ns: 0,
        },
    )?;
    w.act(leader, None)?;
    w.until(|w| {
        w.nodes
            .get(&leader)
            .is_some_and(|n| n.group.configuration().learners.contains(&4))
    })?;
    let node = w.node(leader)?;
    ensure!(node.group.disk_is_slow(other));
    ensure!(node.shared.with(|l| l.report(4).map(|r| r.groups)) == Some(Some(1)));
    node.membership.belief(
        &mut node.group,
        Change::DiskCaughtUp {
            peer: other,
            at_ns: 0,
        },
    )?;
    ensure!(!node.group.disk_is_slow(other));
    Ok(())
}

#[test]
fn no_election_storm() -> Result<()> {
    // A high-latency fleet still runs an entire clock hour in the seeded event scheduler.
    let mut w = Fleet::scaled(&[1, 2, 3], 1000)?;
    let _ = w.settled()?;
    let allowance = |w: &Fleet| -> f64 {
        w.nodes
            .iter()
            .flat_map(|(&id, node)| {
                w.nodes
                    .keys()
                    .copied()
                    .filter(move |p| *p != id)
                    .map(move |p| {
                        node.shared
                            .with(|l| l.report(p).map_or(0.0, |r| r.allowance))
                            .unwrap_or_default()
                    })
            })
            .sum()
    };
    let start = (w.elected.len(), allowance(&w));
    let until = w.now + ns(Duration::from_secs(3600));
    w.until(|w| w.now >= until)?;
    let changes = w.elected.len() - start.0;
    let predicted = allowance(&w) - start.1;
    ensure!(
        changes as f64 <= predicted.ceil(),
        "elections in one hour: {changes}, detector allowance: {predicted}"
    );
    Ok(())
}

/// The three replicas of an additional live session on the same three physical nodes.
/// Only messages produced by this session are routed here.
struct Session(Vec<(Member, Membership)>);
impl Session {
    fn open(w: &Fleet, key: u128) -> Result<Self> {
        let config = ConfState {
            voters: w.nodes.keys().copied().collect(),
            ..Default::default()
        };
        let mut members = Vec::new();
        for (&id, node) in &w.nodes {
            let settings = crate::convo::raft::raft_settings(id, 1)?;
            let machine = Ledger {
                conf: config.clone(),
                ..Default::default()
            };
            let mut r = Replica::open(
                &settings,
                RamStore::new(),
                machine,
                Bytes::new(settings.core.max_size_per_msg.saturating_mul(2)),
            )?;
            let mut m = Membership::new(node.shared.clone(), key, None, true);
            m.reconcile(&mut r, true)?;
            members.push((r, m));
        }
        members
            .first_mut()
            .context("no selected node")?
            .0
            .campaign()?;
        let mut session = Session(members);
        let _ = session.drive(w.now)?;
        ensure!(
            session.0.iter().filter(|(r, _)| r.is_leader()).count() == 1,
            "session {key} did not establish a leader"
        );
        Ok(session)
    }
    fn drive(&mut self, now: u64) -> Result<u64> {
        let mut wire = Vec::new();
        let mut bytes = 0;
        loop {
            let mut work = false;
            for (r, _) in &mut self.0 {
                let mut out = Output::default();
                let driven = r.drive(now, Waker::noop(), &mut out)?;
                work |= driven.more || driven.out != 0;
                for m in out.messages {
                    bytes += m.encoded_len() as u64;
                    wire.push(m);
                }
            }
            if wire.is_empty() && !work {
                return Ok(bytes);
            }
            for m in wire.drain(..) {
                let (r, _) = self
                    .0
                    .iter_mut()
                    .find(|(r, _)| r.id() == m.to)
                    .context("unknown member")?;
                r.step(m)?;
            }
        }
    }
}

fn measure_idle(groups: usize) -> Result<(u64, u64)> {
    let mut w = Fleet::new(&[1, 2, 3])?;
    w.settled()?;
    w.sessions = (1..groups)
        .map(|key| Session::open(&w, key as u128 + 1))
        .collect::<Result<Vec<_>>>()?;
    // This count is maintained by the production attach/detach path.
    ensure!(
        w.nodes
            .get(&1)
            .and_then(|n| n.shared.with(|l| l.report(2).map(|p| p.groups)))
            .flatten()
            == Some(groups as u32)
    );
    let settled = w.now + ns(Duration::from_millis(100));
    w.until(|w| w.now >= settled)?;
    for session in &mut w.sessions {
        session.drive(w.now)?;
    }
    let start = (w.raft, w.heartbeat.clone());
    let end = w.now + ns(Duration::from_secs(1));
    w.until(|w| w.now >= end)?;
    let mut group_bytes = w.raft - start.0;
    for session in &mut w.sessions {
        group_bytes += session.drive(w.now)?;
        ensure!(
            session.0.iter().all(|(r, _)| r.deadline().is_none()),
            "quiescent replica armed a timer"
        );
    }
    for (peer, bytes) in &mut w.heartbeat {
        *bytes -= start.1.get(peer).copied().unwrap_or_default();
    }
    let peer_bytes = w.heartbeat.values().sum();
    eprintln!(
        "{groups} idle sessions, one simulated second: {group_bytes} session bytes, {peer_bytes} signed peer heartbeat bytes, by node {:?}",
        w.heartbeat
    );
    Ok((group_bytes, peer_bytes))
}

#[test]
fn ten_thousand_idle_sessions_send_no_group_bytes() -> Result<()> {
    let (group_bytes, peer_bytes) = measure_idle(10_000)?;
    ensure!(group_bytes == 0 && peer_bytes > 0);
    Ok(())
}

#[test]
fn peer_heartbeat_bytes_do_not_grow_with_session_count() -> Result<()> {
    let hundred = measure_idle(100)?;
    let thousand = measure_idle(1000)?;
    // Each group contributes its measured election cost to the node-pair detector, so
    // adding groups can shift the optimum interval. The increase must cost less than
    // even one additional directed peer stream on this unchanged three-node fleet.
    let directed_pairs = 3 * (3 - 1);
    let one_physical_stream = hundred.1.div_ceil(directed_pairs);
    ensure!(
        hundred.0 == 0
            && thousand.0 == 0
            && hundred.1 > 0
            && thousand.1.saturating_sub(hundred.1) < one_physical_stream,
        "100 sessions {hundred:?}, 1000 sessions {thousand:?}, one stream {one_physical_stream}"
    );
    Ok(())
}

#[test]
fn a_joint_leader_relinquishes_when_only_its_outgoing_majority_survives() -> Result<()> {
    use hyper_raft::proto::{ConfChangeSingle, ConfChangeTransition, ConfChangeType};
    let mut fleet = Fleet::new(&[1, 2, 3, 4, 5])?;
    let leader = fleet.settled()?;
    let others: Vec<_> = fleet
        .nodes
        .keys()
        .copied()
        .filter(|id| *id != leader)
        .collect();
    let removed = others.get(..2).context("outgoing voters missing")?;
    let kept = others.get(2..).context("incoming voters missing")?;
    let change = ConfChangeV2 {
        transition: ConfChangeTransition::Explicit,
        changes: removed
            .iter()
            .map(|&node_id| ConfChangeSingle {
                node_id,
                change_type: ConfChangeType::RemoveNode,
            })
            .collect(),
        ..Default::default()
    };
    fleet.node(leader)?.group.change(Vec::new(), &change)?;
    fleet.act(leader, None)?;
    fleet.until(|w| {
        w.nodes.get(&leader).is_some_and(|n| {
            n.group.configuration().voters_outgoing.len() == 5
                && n.group.configuration().voters.len() == 3
        })
    })?;
    let now = fleet.now;
    let n = fleet.node(leader)?;
    for &peer in kept {
        n.membership
            .belief(&mut n.group, Change::Stopped { peer, at_ns: now })?;
    }
    ensure!(
        !n.group.is_leader(),
        "the incoming configuration has only one healthy voter"
    );
    Ok(())
}

#[test]
fn replacing_the_detector_reattaches_once_and_holds_unmeasured_campaigns() -> Result<()> {
    let mut fleet = Fleet::new(&[1, 2, 3])?;
    let leader = fleet.settled()?;
    let peer = *fleet
        .nodes
        .keys()
        .find(|p| **p != leader)
        .context("missing follower")?;
    let n = fleet.node(peer)?;
    let previous = n.shared.generation();
    ensure!(n.membership.timed());
    n.shared.initialize_for_test(hyper_liveness::Settings {
        local: peer,
        run: 2,
        max_peers: 3,
        resolution: Duration::from_nanos(1),
        history: hyper_timing::Exposure::new(),
    })?;
    n.membership.reconcile(&mut n.group, true)?;
    n.membership.reconcile(&mut n.group, true)?;
    ensure!(n.shared.generation() > previous && !n.membership.timed());
    ensure!(n.shared.with(|l| l.report(leader).map(|r| r.groups)) == Some(Some(1)));
    ensure!(
        n.group.campaign().is_err(),
        "retired clock measurements opened a new election"
    );
    Ok(())
}

#[test]
fn measuring_only_the_incoming_quorum_cannot_open_a_joint_election() -> Result<()> {
    let mut fleet = Fleet::new(&[1, 2, 3, 4])?;
    fleet.cut(1, 3);
    fleet.cut(1, 4);
    let shared = fleet.node(1)?.shared.clone();
    let measured = |peer| {
        shared.with(|l| {
            l.granularity().is_some() && l.round_trip(peer).is_some_and(|r| r.samples() > 0)
        }) == Some(true)
    };
    fleet.until(|_| measured(2))?;
    let settings = crate::convo::raft::raft_settings(1, 1)?;
    let ledger = Ledger {
        conf: ConfState {
            voters: vec![1, 2],
            voters_outgoing: vec![1, 3, 4],
            ..Default::default()
        },
        ..Default::default()
    };
    let mut replica = Replica::open(
        &settings,
        RamStore::new(),
        ledger,
        Bytes::new(settings.core.max_size_per_msg.saturating_mul(4)),
    )?;
    let mut membership = Membership::new(shared.clone(), 2, None, true);
    membership.reconcile(&mut replica, true)?;
    ensure!(!membership.timed() && replica.campaign().is_err());
    fleet.cut.remove(&(1, 3));
    fleet.cut.remove(&(3, 1));
    fleet.until(|_| measured(3))?;
    membership.measure(&mut replica)?;
    ensure!(membership.timed() && replica.campaign().is_ok());
    Ok(())
}

#[test]
fn a_busy_wal_keeps_verdict_order_and_one_pending_calibration_per_peer() {
    use crate::liveness::Event;
    let mut inbox = crate::convo::liveness::Inbox::default();
    assert!(inbox.push(Event::Timing(2)));
    for _ in 0..10_000 {
        assert!(!inbox.push(Event::Timing(2)));
    }
    assert!(!inbox.push(Event::Change(Change::Stopped { peer: 2, at_ns: 1 })));
    assert!(!inbox.push(Event::Change(Change::Restarted { peer: 2, at_ns: 2 })));
    assert!(!inbox.push(Event::Timing(3)));
    let mut batch = Vec::new();
    inbox.take(&mut batch);
    assert!(matches!(
        batch.as_slice(),
        [
            Event::Timing(2),
            Event::Change(Change::Stopped { peer: 2, at_ns: 1 }),
            Event::Change(Change::Restarted { peer: 2, at_ns: 2 }),
            Event::Timing(3)
        ]
    ));
    assert!(inbox.push(Event::Timing(2)));
}

#[test]
fn disk_handover_skips_a_caught_up_voter_which_has_stopped() -> Result<()> {
    let mut w = Fleet::new(&[1, 2, 3, 4, 5])?;
    let owner = w.settled()?;
    let dead = *w
        .nodes
        .keys()
        .find(|&&id| id != owner)
        .context("missing alternate voter")?;
    w.node(dead)?.alive = false;
    w.until(|w| {
        w.nodes.get(&owner).is_some_and(|n| {
            n.shared
                .with(|l| l.trust(dead) == Some(hyper_timing::Trust::Suspected))
                == Some(true)
        })
    })?;
    ensure!(!w.node(owner)?.membership.can_receive_lead(dead));
    let since = w.now;
    w.node(owner)?.stalled = Some(since);
    w.until(|w| w.leader().is_some_and(|id| id != owner && id != dead))?;
    Ok(())
}
