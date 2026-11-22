//! The leader's quorum patience (replication.md, "The leader's quorum patience"; kiln goal
//! 6b112f14729c899e826452260ca618c1): what an owner measured of its voters' answers while it led,
//! in its own ticks, and the ticks beyond its election timeout it waits before it asks whether a
//! quorum heard it (`hyper_raft::Raft::set_quorum_patience`). A pure core: the owner tells it its
//! ticks and the answers it takes, and asks it for the patience before each tick.

use amphora_api::Refusal;

/// How long a window's generation lasts, learned from the events it keeps where it runs
/// (replication.md, "Decision (lane constants): generations learned where they run"): the ticks
/// between its last two events, so what it learned is believed for one to two of its own
/// recurrences, as long as its events keep coming that often and no longer. Its first event's
/// recurrence counts from the tick the window began to watch; before it nothing ages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Recurrence {
    last: Option<u64>,
    length: Option<u64>,
}

/// A window watching from tick 0, its owner's first.
impl Default for Recurrence {
    fn default() -> Self {
        Self {
            last: Some(0),
            length: None,
        }
    }
}

impl Recurrence {
    /// An event at tick `at`; a second at the same tick is the same event.
    pub(super) fn seen(&mut self, at: u64) {
        if let Some(last) = self.last
            && last != at
        {
            self.length = Some(at.saturating_sub(last));
        }
        self.last = Some(at);
    }
    /// Whether a generation begun at tick `began` has ended at tick `at`.
    pub(super) fn ended(&self, began: u64, at: u64) -> bool {
        self.length
            .is_some_and(|length| at.saturating_sub(began) >= length)
    }
    /// The ticks a generation lasts now, none before a second event.
    #[cfg(test)]
    pub(super) fn length(&self) -> Option<u64> {
        self.length
    }
}

/// Voters a leader measures at most: the replicated owner's member bound.
const PEERS: usize = super::replicated::MEMBERS;

/// An answer the window does not learn, and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum Unmeasured {
    /// The host's detector suspected the voter while its answer was awaited: gone, not slow, so
    /// the patience never covers it (the write slot's rule, `slot::Unmeasured::Suspected`).
    #[error("voter {voter} answered after {gap} ticks, suspected by the host's detector meanwhile")]
    Suspected { voter: u64, gap: u64 },
    /// More voters than the owner's member bound.
    #[error("voter {voter} is past the {limit} voters a leader measures")]
    Full { voter: u64, limit: usize },
}

#[derive(Debug, Default)]
struct Peer {
    id: u64,
    /// The term of `last`: a gap is counted only within one term this owner leads.
    term: u64,
    last: u64,
    /// The longest gap it ended in this generation, and in the one before.
    current: u64,
    before: u64,
    /// The tick its generation began, and how long one lasts: the ticks between its last two
    /// gaps past the election timeout, the gaps its patience covers.
    generation: u64,
    recurrence: Recurrence,
    /// The host's detector suspected it since its last answer.
    suspected: bool,
}

/// One owner's measure of its voters' answers.
#[derive(Debug)]
pub(super) struct Answers {
    ticks: u64,
    /// The core's election timeout, in ticks: a gap past it is one the patience covers.
    election: u64,
    /// The term this owner leads, and the tick it began to.
    leading: Option<u64>,
    led_from: u64,
    peers: Vec<Peer>,
    tails: Vec<u64>,
}

impl Answers {
    /// An owner's measure, its core's election timeout `election` ticks.
    pub(super) fn new(election: usize) -> Self {
        Self {
            ticks: 0,
            election: u64::try_from(election).unwrap_or(u64::MAX),
            leading: None,
            led_from: 0,
            peers: Vec::new(),
            tails: Vec::new(),
        }
    }

    /// One tick of the owner, leading the term `leading` if it leads, among `voters` and, in a
    /// joint configuration, the `outgoing` voters. A voter that left both is forgotten.
    pub(super) fn tick(
        &mut self,
        leading: Option<u64>,
        voters: &[u64],
        outgoing: &[u64],
    ) -> Result<(), Refusal> {
        self.ticks = self
            .ticks
            .checked_add(1)
            .ok_or_else(|| Refusal::Unavailable("the owner's tick count overflowed".into()))?;
        self.peers
            .retain(|peer| voters.contains(&peer.id) || outgoing.contains(&peer.id));
        let ticks = self.ticks;
        for peer in &mut self.peers {
            if peer.recurrence.ended(peer.generation, ticks) {
                peer.generation = ticks;
                peer.before = std::mem::take(&mut peer.current);
            }
        }
        if leading != self.leading {
            self.leading = leading;
            self.led_from = self.ticks;
        }
        Ok(())
    }

    /// The host's detector suspects voter `voter`: its next gap is refused, not learned. One past
    /// the voters the leader measures is not marked, and its answer is refused `Full` all the same.
    pub(super) fn suspected(&mut self, voter: u64) {
        if let Ok(index) = self.peer(voter)
            && let Some(peer) = self.peers.get_mut(index)
        {
            peer.suspected = true;
        }
    }

    /// Voter `voter`'s place among the peers, added if new.
    fn peer(&mut self, voter: u64) -> Result<usize, Unmeasured> {
        Ok(match self.peers.iter().position(|peer| peer.id == voter) {
            Some(index) => index,
            None if self.peers.len() >= PEERS => {
                return Err(Unmeasured::Full {
                    voter,
                    limit: PEERS,
                });
            }
            None => {
                // Its recurrence counts from the tick the leader began to measure it, so its
                // first gap past the election timeout is believed for as long as it took to come.
                self.peers.push(Peer {
                    id: voter,
                    generation: self.ticks,
                    recurrence: Recurrence {
                        last: Some(self.ticks),
                        length: None,
                    },
                    ..Peer::default()
                });
                self.peers.len().saturating_sub(1)
            }
        })
    }

    /// Voter `voter` answered the leader of `term`. Counted only in the term this owner leads:
    /// the gap since its last answer in that term, or since the tick the owner began to lead.
    pub(super) fn heard(&mut self, voter: u64, term: u64) -> Result<(), Unmeasured> {
        if self.leading != Some(term) {
            return Ok(());
        }
        let index = self.peer(voter)?;
        let Some(peer) = self.peers.get_mut(index) else {
            return Ok(());
        };
        let since = if peer.term == term {
            peer.last
        } else {
            self.led_from
        };
        let gap = self.ticks.saturating_sub(since);
        (peer.term, peer.last) = (term, self.ticks);
        if std::mem::take(&mut peer.suspected) {
            return Err(Unmeasured::Suspected { voter, gap });
        }
        if gap > self.election {
            peer.recurrence.seen(self.ticks);
        }
        peer.current = peer.current.max(gap);
        Ok(())
    }

    /// The ticks beyond `election` this owner, `local`, waits before its quorum check: the
    /// larger of what `voters` and, in a joint configuration, `outgoing` need, less the election
    /// timeout, never below zero. A check needs a majority of each set.
    pub(super) fn patience(
        &mut self,
        voters: &[u64],
        outgoing: &[u64],
        local: u64,
        election: usize,
    ) -> usize {
        let tail = self.tail(voters, local).max(self.tail(outgoing, local));
        usize::try_from(tail)
            .unwrap_or(usize::MAX)
            .saturating_sub(election)
    }

    /// The gap a majority of `set` answers within: the k-th smallest tail of its members other
    /// than `local`, k being the majority less `local` where it is a member. A member not
    /// measured has a tail of zero; an empty set needs no one.
    fn tail(&mut self, set: &[u64], local: u64) -> u64 {
        let majority = set
            .len()
            .checked_div(2)
            .map_or(0, |half| half.saturating_add(1));
        let needed = if set.contains(&local) {
            majority.saturating_sub(1)
        } else {
            majority
        };
        let Some(rank) = needed.checked_sub(1) else {
            return 0;
        };
        self.tails.clear();
        for member in set.iter().filter(|member| **member != local).take(PEERS) {
            let tail = self
                .peers
                .iter()
                .find(|peer| peer.id == *member)
                .map_or(0, |peer| peer.current.max(peer.before));
            self.tails.push(tail);
        }
        self.tails.sort_unstable();
        self.tails.get(rank).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ELECTION: usize = 20;
    const VOTERS: [u64; 3] = [1, 2, 3];

    fn leading(answers: &mut Answers, ticks: usize) {
        for _ in 0..ticks {
            answers.tick(Some(7), &VOTERS, &[]).unwrap();
        }
    }

    /// Kiln goal 6b112f14 step 3: do two followers answer 30 and 40 ticks apart; expect the
    /// patience of the smaller, 30 less the election timeout.
    #[test]
    fn the_patience_is_the_quorums_tail_less_the_election_timeout() {
        let mut answers = Answers::new(ELECTION);
        leading(&mut answers, 1);
        answers.heard(2, 7).unwrap();
        answers.heard(3, 7).unwrap();
        leading(&mut answers, 30);
        answers.heard(2, 7).unwrap();
        leading(&mut answers, 10);
        answers.heard(3, 7).unwrap();
        assert_eq!(answers.patience(&VOTERS, &[], 1, ELECTION), 10);
    }

    /// Do nothing but answer every tick; expect no patience: a leader of no stalls checks at its
    /// election timeout as at fixed ticks.
    #[test]
    fn answers_within_the_election_timeout_give_no_patience() {
        let mut answers = Answers::new(ELECTION);
        for _ in 0..100 {
            leading(&mut answers, 1);
            answers.heard(2, 7).unwrap();
            answers.heard(3, 7).unwrap();
        }
        assert_eq!(answers.patience(&VOTERS, &[], 1, ELECTION), 0);
    }

    /// T-GN.1.1: do the host's detector suspect a follower, which then answers 200 ticks late;
    /// expect its gap refused, typed, and not learned, the other's learned; then expect its next
    /// gap, unsuspected, learned.
    #[test]
    fn a_suspected_voters_gap_is_refused_and_not_learned() {
        let mut answers = Answers::new(ELECTION);
        leading(&mut answers, 1);
        answers.heard(2, 7).unwrap();
        answers.heard(3, 7).unwrap();
        answers.suspected(2);
        leading(&mut answers, 200);
        assert_eq!(
            answers.heard(2, 7),
            Err(Unmeasured::Suspected { voter: 2, gap: 200 })
        );
        answers.heard(3, 7).unwrap();
        assert_eq!(answers.patience(&VOTERS, &[], 1, ELECTION), 0);
        leading(&mut answers, 40);
        answers.heard(2, 7).unwrap();
        answers.heard(3, 7).unwrap();
        assert_eq!(answers.patience(&VOTERS, &[], 1, ELECTION), 20);
    }

    /// T-GN.1.1: do a gap of 50 ticks, prompt answers, and a second gap of 50 ticks 99 ticks after
    /// the first; expect a generation of 99 ticks, the second gap believed through one more
    /// generation and forgotten within two.
    #[test]
    fn a_gap_is_believed_for_one_to_two_of_its_voters_own_recurrences() {
        let mut answers = Answers::new(ELECTION);
        leading(&mut answers, 1);
        answers.heard(2, 7).unwrap();
        answers.heard(3, 7).unwrap();
        let prompt = |answers: &mut Answers, ticks: usize| {
            for _ in 0..ticks {
                leading(answers, 1);
                answers.heard(2, 7).unwrap();
                answers.heard(3, 7).unwrap();
            }
        };
        let gap = |answers: &mut Answers| {
            leading(answers, 50);
            answers.heard(2, 7).unwrap();
            answers.heard(3, 7).unwrap();
        };
        gap(&mut answers);
        prompt(&mut answers, 49);
        gap(&mut answers);
        let peer = answers.peers.iter().find(|peer| peer.id == 2).unwrap();
        assert_eq!(peer.recurrence.length(), Some(99));
        prompt(&mut answers, 98);
        assert_eq!(answers.patience(&VOTERS, &[], 1, ELECTION), 30);
        prompt(&mut answers, 100);
        assert_eq!(answers.patience(&VOTERS, &[], 1, ELECTION), 0);
    }

    /// T-GN.1.1: do a recurrence see events at ticks 0, 0 again, 10 and 30; expect no length
    /// until its second tick, then 10, then 20, and a generation begun at 30 ended at 50.
    #[test]
    fn a_recurrence_is_the_time_between_its_last_two_events() {
        let mut recurrence = Recurrence::default();
        recurrence.seen(0);
        assert_eq!(recurrence.length(), None);
        recurrence.seen(10);
        assert_eq!(recurrence.length(), Some(10));
        recurrence.seen(30);
        assert_eq!(recurrence.length(), Some(20));
        assert!(!recurrence.ended(30, 49));
        assert!(recurrence.ended(30, 50));
    }

    /// Do answers while following, and an answer of an earlier term; expect neither learned, and
    /// a new lead's first gap counted from the tick it began.
    #[test]
    fn only_the_term_it_leads_is_measured() {
        let mut answers = Answers::new(ELECTION);
        for _ in 0..60 {
            answers.tick(None, &VOTERS, &[]).unwrap();
        }
        answers.heard(2, 7).unwrap();
        answers.heard(3, 7).unwrap();
        leading(&mut answers, 25);
        answers.heard(2, 6).unwrap();
        answers.heard(3, 6).unwrap();
        assert_eq!(answers.patience(&VOTERS, &[], 1, ELECTION), 0);
        answers.heard(2, 7).unwrap();
        answers.heard(3, 7).unwrap();
        // The lead began at its first tick, 24 ticks before these answers.
        assert_eq!(answers.patience(&VOTERS, &[], 1, ELECTION), 4);
    }

    /// Do a voter answer late, then leave the configuration; expect it forgotten.
    #[test]
    fn a_voter_that_left_is_forgotten() {
        let mut answers = Answers::new(ELECTION);
        leading(&mut answers, 1);
        answers.heard(2, 7).unwrap();
        answers.heard(3, 7).unwrap();
        leading(&mut answers, 40);
        answers.heard(2, 7).unwrap();
        answers.heard(3, 7).unwrap();
        answers.tick(Some(7), &[1, 3], &[]).unwrap();
        assert_eq!(answers.patience(&[1, 3], &[], 1, ELECTION), 20);
        assert_eq!(answers.patience(&VOTERS, &[], 1, ELECTION), 0);
    }

    /// Do, in a joint configuration from {1, 2, 3} to {1, 4, 5}, the outgoing set's followers
    /// answer 50 ticks apart and the incoming set's at once; expect the patience the outgoing set
    /// needs, since a check needs a majority of each. Then expect a voter that left both forgotten.
    #[test]
    fn a_joint_configuration_takes_the_larger_of_its_two_sets() {
        let incoming = [1, 4, 5];
        let joint = |answers: &mut Answers| answers.tick(Some(7), &incoming, &VOTERS).unwrap();
        let mut answers = Answers::new(ELECTION);
        joint(&mut answers);
        for voter in [2, 3, 4, 5] {
            answers.heard(voter, 7).unwrap();
        }
        for _ in 0..50 {
            joint(&mut answers);
            answers.heard(4, 7).unwrap();
            answers.heard(5, 7).unwrap();
        }
        answers.heard(2, 7).unwrap();
        answers.heard(3, 7).unwrap();
        assert_eq!(answers.patience(&incoming, &VOTERS, 1, ELECTION), 30);
        assert_eq!(answers.patience(&incoming, &[], 1, ELECTION), 0);
        answers.tick(Some(7), &incoming, &[]).unwrap();
        assert_eq!(answers.patience(&incoming, &VOTERS, 1, ELECTION), 0);
    }

    /// Do a leader the incoming set leaves out (it is removed) see its incoming voters answer 40
    /// and 50 ticks apart; expect a majority of the incoming set, both of them, so 50.
    #[test]
    fn a_leader_outside_a_set_needs_a_majority_of_its_members() {
        let incoming = [2, 3];
        let mut answers = Answers::new(ELECTION);
        answers.tick(Some(7), &incoming, &VOTERS).unwrap();
        answers.heard(2, 7).unwrap();
        answers.heard(3, 7).unwrap();
        for _ in 0..40 {
            answers.tick(Some(7), &incoming, &VOTERS).unwrap();
        }
        answers.heard(2, 7).unwrap();
        for _ in 0..10 {
            answers.tick(Some(7), &incoming, &VOTERS).unwrap();
        }
        answers.heard(3, 7).unwrap();
        assert_eq!(answers.patience(&incoming, &[], 1, ELECTION), 30);
    }

    /// A single voter needs no one: no patience.
    #[test]
    fn a_lone_voter_has_no_patience() {
        let mut answers = Answers::new(ELECTION);
        assert_eq!(answers.patience(&[1], &[], 1, ELECTION), 0);
    }
}
