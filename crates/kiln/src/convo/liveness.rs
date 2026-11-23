//! CP-T3.2. The membership and disk evidence one durable group contributes to the node's
//! shared detector. Only an actual membership change creates or removes a pair reference.
use hyper_durable::{Budget, Driven, LogStore, Replica, ReplicaError, StateMachine};
use hyper_liveness::Change;
use hyper_timing::{PathEstimate, Trust};
use std::sync::Arc;
use std::time::Duration;

/// The socket thread only enqueues evidence. Recalibration is read from the latest
/// pair measurements; process verdicts and restarts retain their delivery order.
#[derive(Default)]
pub(crate) struct Inbox {
    events: Vec<crate::liveness::Event>,
    timing: std::collections::HashSet<u64>,
}
impl Inbox {
    pub fn push(&mut self, event: crate::liveness::Event) -> bool {
        if matches!(event,crate::liveness::Event::Timing(peer) if !self.timing.insert(peer)) {
            return false;
        }
        let wake = self.events.is_empty();
        self.events.push(event);
        wake
    }
    pub fn take(&mut self, empty: &mut Vec<crate::liveness::Event>) {
        std::mem::swap(&mut self.events, empty);
        self.timing.clear();
    }
}

pub(crate) struct Membership {
    pub shared: Arc<crate::liveness::Shared>,
    group: u128,
    generation: u64,
    pub revision: u64,
    voters: Vec<u64>,
    outgoing: Vec<u64>,
    executable: bool,
    holding: Option<bool>,
    pub peers: Vec<(u64, u64)>,
    /// Fixed Raft member 1 of kiln.db can have a different authenticated physical peer ID.
    alias: Option<(u64, u64)>,
    pending: Option<u64>,
    election: Option<Duration>,
}

impl Membership {
    pub fn new(
        shared: Arc<crate::liveness::Shared>,
        group: u128,
        alias: Option<(u64, u64)>,
        executable: bool,
    ) -> Self {
        Self {
            shared,
            group,
            generation: 0,
            revision: 0,
            voters: Vec::new(),
            outgoing: Vec::new(),
            executable,
            holding: None,
            alias,
            peers: Vec::new(),
            pending: None,
            election: None,
        }
    }

    fn physical(&self, member: u64) -> u64 {
        self.alias
            .filter(|(r, _)| *r == member)
            .map_or(member, |(_, p)| p)
    }

    fn hold<L: LogStore, M: StateMachine, B: Budget>(
        &mut self,
        replica: &mut Replica<L, M, B>,
        held: bool,
    ) -> Result<(), ReplicaError> {
        if self.holding != Some(held) {
            replica.hold_campaigns(held)?;
            self.holding = Some(held);
        }
        Ok(())
    }

    pub fn reconcile<L: LogStore, M: StateMachine, B: Budget>(
        &mut self,
        replica: &mut Replica<L, M, B>,
        force: bool,
    ) -> anyhow::Result<bool> {
        let configuration = replica.configuration();
        let electorate =
            self.voters != configuration.voters || self.outgoing != configuration.voters_outgoing;
        let sole = configuration.voters.as_slice() == [replica.id()]
            && configuration.voters_outgoing.is_empty();
        let unchanged = !electorate
            && replica.peers().count() == self.peers.len()
            && replica
                .peers()
                .all(|m| self.peers.binary_search_by_key(&m, |(r, _)| *r).is_ok());
        if unchanged && self.generation == self.shared.generation() && !force {
            return Ok(false);
        }
        if electorate || self.generation != self.shared.generation() {
            self.hold(replica, !self.executable || !sole)?;
            self.voters.clone_from(&replica.configuration().voters);
            self.outgoing
                .clone_from(&replica.configuration().voters_outgoing);
        }
        let mut next: Vec<_> = replica.peers().map(|m| (m, self.physical(m))).collect();
        next.sort_unstable();
        next.dedup();
        if !unchanged && self.election.take().is_some() {
            // A different electorate may have no measured quorum yet, even when its
            // physical members are unchanged (learners promoted, or old voters removed).
            for &(_, physical) in &self.peers {
                self.shared.set_group_election(self.group, physical, None);
            }
        }
        let attached = self
            .shared
            .with(|l| -> anyhow::Result<bool> {
                let generation = self.shared.generation();
                let replaced = self.generation != generation;
                if replaced {
                    // References belonged to the retired stream, whose pairs no longer exist.
                    self.peers.clear();
                    self.election = None;
                    self.generation = generation;
                }
                for &(raw, physical) in &next {
                    let added = self.peers.binary_search(&(raw, physical)).is_err();
                    if added {
                        l.attach(physical)?;
                        self.peers.push((raw, physical));
                        self.peers.sort_unstable();
                    }
                    if (added || force || replaced) && !(force && self.alias.is_none()) {
                        if raw == physical {
                            replica.believe_of(l, raw)?;
                        } else {
                            if l.disk_slow(physical) == Some(true) {
                                replica.disk_slow(raw)?;
                            } else {
                                replica.disk_caught_up(raw)?;
                            }
                            if l.stopped(physical) {
                                replica.stopped(raw)?;
                            } else if l.trust(physical) == Some(Trust::Suspected) {
                                replica.suspect(raw)?;
                            } else {
                                replica.trust(raw)?;
                            }
                        }
                    }
                }
                self.peers.retain(|pair| {
                    if next.binary_search(pair).is_ok() {
                        true
                    } else {
                        let _ = l.detach(pair.1);
                        false
                    }
                });
                if force && self.alias.is_none() {
                    replica.believe_all(l)?;
                }
                replica.resign_without_quorum()?;
                Ok(true)
            })
            .transpose()?
            .unwrap_or(false);
        if attached {
            self.revision = self.revision.wrapping_add(1);
            self.measure(replica)?;
            self.shared.changed();
        } else {
            if !unchanged {
                self.peers = next;
                self.revision = self.revision.wrapping_add(1);
            }
            // The running socket can retire before its replacement is initialized.
            // No old timetable or old attachment may start a ballot in that interval.
            self.hold(replica, !self.executable || !sole)?;
            if self.election.take().is_some() {
                for &(_, peer) in &self.peers {
                    self.shared.set_group_election(self.group, peer, None);
                }
            }
        }
        Ok(attached)
    }

    pub fn measure<L: LogStore, M: StateMachine, B: Budget>(
        &mut self,
        replica: &mut Replica<L, M, B>,
    ) -> anyhow::Result<()> {
        let span = self
            .shared
            .with(|l| -> anyhow::Result<Option<Duration>> {
                let conf = replica.configuration();
                if conf.voters_outgoing.is_empty()
                    && let Some(span) = replica.measure(l)?
                {
                    return Ok(Some(span.election));
                }
                // kiln.db member 1 names the original server in its WAL; the detector knows its
                // physical identity. A sole voter needs no election, but its replication retries to
                // learners use their measured paths (the core arms those only with work in flight).
                let conf = replica.configuration();
                let Some(granularity) = l.granularity() else {
                    return Ok(None);
                };
                let individual = |voters: &[u64], learners: bool| {
                    let paths = self
                        .peers
                        .iter()
                        .filter(|(raw, _)| learners || voters.contains(raw))
                        .filter_map(|(_, p)| l.round_trip(*p));
                    let count = if learners {
                        self.peers.len().saturating_add(1)
                    } else {
                        voters.len()
                    };
                    let durable = l.flush_mean().unwrap_or_default();
                    let outside = !learners && !voters.contains(&replica.id());
                    let mut ballot = hyper_timing::Ballot::measure(
                        paths.clone(),
                        count.max(2),
                        durable,
                        granularity,
                    )?;
                    if outside {
                        // An outgoing half may exclude this candidate. Find the
                        // first measured arrival at which its actual majority can vote.
                        let means = paths.filter(|p| p.samples() > 0).map(|p| p.mean_ns());
                        let arrival = means
                            .clone()
                            .filter(|&t| means.clone().filter(|&x| x <= t).count() > count / 2)
                            .min()?;
                        ballot.round = Duration::from_nanos(arrival).saturating_add(durable);
                    }
                    Some(hyper_raft::Timing::of(&ballot, &ballot.span(granularity)?))
                };
                let mut timing: Option<hyper_raft::Timing> = None;
                let joint = !conf.voters_outgoing.is_empty();
                for voters in [&conf.voters, &conf.voters_outgoing] {
                    if voters.is_empty() || (joint && voters.as_slice() == [replica.id()]) {
                        continue;
                    }
                    let Some(half) = individual(voters, !joint && voters.len() == 1) else {
                        return Ok(None);
                    };
                    let t = timing.get_or_insert(half);
                    t.span = t.span.max(half.span);
                    t.round = t.round.max(half.round);
                    t.election = t.election.max(half.election);
                }
                if let Some(timing) = timing {
                    replica.set_timing(timing)?;
                    return Ok(Some(timing.election));
                }
                Ok(None)
            })
            .transpose()?
            .flatten();
        let sole = replica.configuration().voters.as_slice() == [replica.id()]
            && replica.configuration().voters_outgoing.is_empty();
        self.hold(replica, !self.executable || (!sole && span.is_none()))?;
        if span.is_none() && self.election.take().is_some() {
            for &(_, p) in &self.peers {
                self.shared.set_group_election(self.group, p, None);
            }
        }
        if let Some(span) = span
            && self.election != Some(span)
        {
            for &(_, peer) in &self.peers {
                self.shared.set_group_election(self.group, peer, Some(span));
            }
            self.election = Some(span);
        }
        Ok(())
    }

    pub fn timed(&self) -> bool {
        self.election.is_some()
    }
    pub fn election(&self) -> Option<Duration> {
        self.election
    }

    pub fn belief<L: LogStore, M: StateMachine, B: Budget>(
        &self,
        replica: &mut Replica<L, M, B>,
        change: Change,
    ) -> Result<(), ReplicaError> {
        let peer = change.peer();
        let Some(&(member, _)) = self.peers.iter().find(|p| p.1 == peer) else {
            return Ok(());
        };
        match change {
            Change::Suspected(_) => {
                replica.suspect(member)?;
                replica.resign_without_quorum()
            }
            Change::Trusted { .. } => replica.trust(member),
            Change::Restarted { .. } => replica.restarted(member),
            Change::Stopped { .. } => {
                replica.stopped(member)?;
                replica.resign_without_quorum()
            }
            Change::DiskSlow { .. } => replica.disk_slow(member),
            Change::DiskCaughtUp { .. } => replica.disk_caught_up(member),
            Change::ModeDiffers { .. } | Change::ModeAgrees { .. } => Ok(()),
        }
    }

    pub fn can_receive_lead(&self, member: u64) -> bool {
        let p = self.physical(member);
        self.shared.with(|l| {
            !l.stopped(p)
                && matches!(l.trust(p), Some(Trust::Trusted { .. }))
                && l.disk_slow(p) != Some(true)
        }) == Some(true)
    }

    /// CP-T3.2. The detector's verdict that `member` can't take the lead: it's gone ([`lost`](Self::lost))
    /// or its disk is slow. A link with no verdict yet is none of these: its first verdict comes as
    /// an event.
    pub fn refuses_lead(&self, member: u64) -> bool {
        let p = self.physical(member);
        self.lost(member) || self.shared.with(|l| l.disk_slow(p) == Some(true)) == Some(true)
    }

    /// The detector's verdict that `member` is gone: it stopped or it's suspected. A link with no
    /// verdict yet is neither.
    pub fn lost(&self, member: u64) -> bool {
        let p = self.physical(member);
        self.shared.with(|l| l.stopped(p) || l.trust(p) == Some(Trust::Suspected)) == Some(true)
    }

    /// AD-1 P1.2. A log with a slow local device can hand its lead to an up-to-date
    /// voter whose disk has room. The execution owner is an independent committed field.
    pub fn yield_stalled_log<L: LogStore, M: StateMachine, B: Budget>(
        &self,
        replica: &mut Replica<L, M, B>,
        eligible: impl Fn(u64) -> bool,
    ) -> Result<bool, ReplicaError> {
        if !replica.is_leader() || self.shared.with(|l| l.own_disk_slow()) != Some(true) {
            return Ok(false);
        }
        let heir = replica.configuration().voters.iter().copied().find(|peer| {
            *peer != replica.id()
                && eligible(*peer)
                && !replica.disk_is_slow(*peer)
                && replica.caught_up(*peer)
                && self.can_receive_lead(*peer)
        });
        if let Some(heir) = heir {
            replica.transfer(heir)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn io(&mut self, driven: &Driven) {
        if driven.flushed.is_some() || self.pending != driven.out_since {
            self.shared
                .report_io(self.group, driven.flushed, driven.out_since);
            self.pending = driven.out_since;
        }
    }
}

impl Drop for Membership {
    fn drop(&mut self) {
        for &(_, peer) in &self.peers {
            self.shared.set_group_election(self.group, peer, None);
        }
        self.shared.with(|l| {
            if self.generation != self.shared.generation() {
                return;
            }
            for (_, peer) in self.peers.drain(..) {
                let _ = l.detach(peer);
            }
        });
        self.shared.forget_group_io(self.group);
        self.shared.changed();
    }
}
