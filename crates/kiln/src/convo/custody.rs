//! AD-1. A Raft term proves who leads its log. Independent process verdicts from a
//! quorum authorize moving the execution owner when the two leadership roles differ.
use hyper_raft::proto::ConfState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const MARKER: &[u8; 4] = b"KPV1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct Query {
    pub owner: u64,
    pub epoch: u64,
    pub run: u64,
    pub term: u64,
    pub leader_run: u64,
    pub origin: Option<[u8; 32]>,
    pub nonce: u64,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(super) enum Wire {
    Check(Query),
    Answer {
        query: Query,
        witness_run: u64,
        serial: u64,
        lost: bool,
    },
}

impl Wire {
    pub fn context(self) -> anyhow::Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(MARKER.len() + size_of::<Self>());
        bytes.extend_from_slice(MARKER);
        Ok(postcard::to_extend(&self, bytes)?)
    }
    pub fn read(data: &[u8]) -> Option<Self> {
        let (frame, rest) = postcard::take_from_bytes(data.strip_prefix(MARKER)?).ok()?;
        rest.is_empty().then_some(frame)
    }
}

pub(super) fn voters(conf: &ConfState) -> impl Iterator<Item = u64> + '_ {
    conf.voters
        .iter()
        .chain(
            conf.voters_outgoing
                .iter()
                .filter(|v| !conf.voters.contains(v)),
        )
        .copied()
}
fn quorum(conf: &ConfState, matches: impl Fn(&u64) -> bool) -> bool {
    [&conf.voters, &conf.voters_outgoing]
        .into_iter()
        .all(|set| set.is_empty() || set.iter().filter(|id| matches(id)).count() > set.len() / 2)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Certificate {
    pub query: Query,
    pub attestations: Vec<(u64, u64)>,
}
impl Certificate {
    pub fn covers(&self, conf: &ConfState, leader: u64) -> bool {
        let q = self.query;
        self.attestations
            .windows(2)
            .all(|pair| matches!(pair,[(a,_),(b,_)] if a<b))
            && self.attestations.contains(&(leader, q.leader_run))
            && self
                .attestations
                .iter()
                .all(|(v, r)| *r != 0 && *v != q.owner && voters(conf).any(|id| id == *v))
            && quorum(conf, |id| {
                self.attestations
                    .binary_search_by_key(id, |(node, _)| *node)
                    .is_ok()
            })
    }
}
pub(super) struct Attestation {
    pub run: u64,
    pub serial: u64,
    pub lost: bool,
}

pub(super) struct Round {
    pub query: Query,
    pub voters: Vec<u64>,
    pub outgoing: Vec<u64>,
    pub started: u64,
    pub window: u64,
    pub retry: Option<u64>,
    pub backoff: u64,
    pub servicing: u64,
    pub asked: BTreeSet<u64>,
    pub answers: BTreeMap<u64, Attestation>,
}
impl Round {
    pub fn confirmed(&self, conf: &ConfState, local: u64, now: u64) -> bool {
        now.saturating_sub(self.started) < self.window
            && quorum(conf, |v| {
                *v == local || self.answers.get(v).is_some_and(|vote| vote.lost)
            })
    }
    pub fn certificate(&self, local: u64) -> Certificate {
        let mut votes = Vec::with_capacity(1 + self.answers.len());
        votes.push((local, self.query.leader_run));
        votes.extend(
            self.answers
                .iter()
                .filter(|(_, v)| v.lost)
                .map(|(id, v)| (*id, v.run)),
        );
        votes.sort_unstable();
        Certificate {
            query: self.query,
            attestations: votes,
        }
    }
    pub fn possible(&self, conf: &ConfState) -> bool {
        quorum(conf, |v| {
            *v != self.query.owner && self.answers.get(v).is_none_or(|vote| vote.lost)
        })
    }
    pub fn answer(
        &mut self,
        conf: &ConfState,
        peer: u64,
        run: u64,
        serial: u64,
        lost: bool,
        now: u64,
    ) {
        if self
            .answers
            .get(&peer)
            .is_some_and(|previous| previous.run == run && previous.serial >= serial)
        {
            return;
        }
        let old = self
            .answers
            .insert(peer, Attestation { run, serial, lost })
            .map(|vote| vote.lost);
        let elapsed = now.saturating_sub(self.started);
        if old.is_none() {
            // Only the first answer measures the application route. A later change
            // during a parked watch measures a process's lifetime, not its link.
            self.servicing = self.servicing.max(elapsed);
        }
        if !self.possible(conf) {
            self.retry = None;
        } else if lost && ((self.retry.is_none() && old != Some(lost)) || elapsed >= self.window) {
            // Late or newly positive evidence wakes another fresh challenge. None of
            // these samples are combined with a vote left over from a prior challenge.
            self.retry = Some(now);
        }
    }
}

pub(super) struct Watch {
    pub leader: u64,
    pub query: Query,
    pub answered: Option<bool>,
    pub serial: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cp_t32_joint_process_custody_needs_both_majorities_and_rejects_old_answers() {
        let conf = ConfState {
            voters: vec![1, 2, 3],
            voters_outgoing: vec![3, 4, 5],
            ..Default::default()
        };
        let now = 100;
        let window = 20;
        let q = Query {
            owner: 5,
            epoch: 2,
            run: 7,
            term: 3,
            leader_run: 1,
            origin: None,
            nonce: 19,
        };
        let mut round = Round {
            query: q,
            voters: conf.voters.clone(),
            outgoing: conf.voters_outgoing.clone(),
            started: now,
            window,
            backoff: window,
            servicing: 0,
            retry: Some(now + window),
            asked: BTreeSet::new(),
            answers: BTreeMap::new(),
        };
        round.answer(&conf, 2, 1, 1, true, now + 1);
        assert!(!round.confirmed(&conf, 1, now + 2));
        round.answer(&conf, 3, 1, 1, true, now + 1);
        round.answer(&conf, 4, 1, 2, false, now + 1);
        round.answer(&conf, 4, 1, 1, true, now + 1);
        assert!(round.retry.is_none() && !round.confirmed(&conf, 1, now + 2));
        round.answer(&conf, 4, 1, 3, true, now + 1);
        let certificate = round.certificate(1);
        assert!(certificate.covers(&conf, 1));
        let changed = ConfState {
            voters: vec![3, 5, 6],
            ..Default::default()
        };
        assert!(!certificate.covers(&changed, 1));
        assert!(round.retry.is_some_and(|due| due <= now + 2));
    }
    #[test]
    fn cp_t32_a_delayed_witness_only_measures_the_next_challenge_window() {
        let conf = ConfState {
            voters: vec![1, 2, 3],
            ..Default::default()
        };
        let q = Query {
            owner: 3,
            run: 42,
            epoch: 8,
            term: 5,
            leader_run: 2,
            origin: None,
            nonce: 9,
        };
        let mut r = Round {
            query: q,
            voters: conf.voters.clone(),
            outgoing: vec![],
            started: 100,
            window: 10,
            backoff: 40,
            servicing: 0,
            retry: Some(140),
            asked: [2].into(),
            answers: BTreeMap::new(),
        };
        r.answer(&conf, 2, 1, 1, true, 131);
        assert!(!r.confirmed(&conf, 1, 131));
        assert_eq!((r.retry, r.servicing), (Some(131), 31));
        r.answer(&conf, 2, 1, 2, false, 160);
        assert!(r.retry.is_none() && !r.confirmed(&conf, 1, 160));
        r.answer(&conf, 2, 1, 3, true, 300);
        assert_eq!((r.retry, r.servicing), (Some(300), 31));
    }
}
