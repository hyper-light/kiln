//! The owner's tick pace, learned where it runs (replication.md, "Decision (lane constants): the
//! owner's tick pace"): its voters' round trips, measured from the replication messages' own sends
//! and answers; the log's flushes; the leader's heartbeats' lateness as they arrive; and the
//! lateness of the owner's own timed waits. hyper-timing's law turns them into the period at which
//! the core's fixed tick counts cover the election the group measures (`TickPace::derive` over
//! `ElectionTiming::derive`, `docs/timing.md` §2.1–§2.3 in vendor/hyper-raft). A pure core: the
//! owner tells it what it sent, took and waited at its own clock's nanoseconds, and asks it for
//! the pace.
//!
//! Before it has measured a quorum's paths, its timer and the leader's link, the pace is
//! hyper-timing's start state, `TickPace::floor` ("the pace with nothing measured: the configured
//! period", vendor/hyper-raft/crates/hyper-timing/src/lib.rs): the period the owner opened with.
//! Once measured, the period follows the measurements, down to the timer's measured granularity
//! and up as far as the measured election needs: the start period bounds neither.

use std::collections::BTreeMap;
use std::time::Duration;

use hyper_raft::proto::{Message, MessageType};
use hyper_timing::{
    Arrivals, Ballot, Costs, ElectionTiming, ExchangeRtt, Exposure, Flushes, Lateness, TickPace,
    arrival_detector_at,
};

/// One kind of request to one peer, timed by its answer. Its answers come back in the order its
/// requests left (the transport's order), so the answer to a timed request is the one that comes
/// once every request sent before it is answered. A request is not timed while one is (Karn and
/// Partridge, "Improving Round-Trip Time Estimates in Reliable Transport Protocols", SIGCOMM 1987:
/// sample only answers whose request is unambiguous). A lost request shifts the count by one, so
/// a later answer is taken for the timed one's: an overestimate, the safe side for an election
/// timeout, and gone once the peer is judged lost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Exchange {
    /// The timed request: when it left, and the answers still owed to requests before it.
    timed: Option<(u64, u32)>,
    /// Requests sent and not answered.
    unanswered: u32,
}

impl Exchange {
    /// A request left at `now`. `lost` is how many unanswered requests the core itself takes for
    /// a lost peer (its election timeout's beats for heartbeats, one for a campaign's request): at
    /// that many the count starts again, as the core's own judgement does.
    pub(super) fn sent(&mut self, now: u64, lost: u32) {
        if self.unanswered >= lost.max(1) {
            *self = Self::default();
        }
        if self.timed.is_none() {
            self.timed = Some((now, self.unanswered));
        }
        self.unanswered = self.unanswered.saturating_add(1);
    }
    /// An answer came at `now`: the timed request's round trip, if it is the one answered.
    pub(super) fn answered(&mut self, now: u64) -> Option<u64> {
        self.unanswered = self.unanswered.saturating_sub(1);
        match self.timed {
            Some((sent, 0)) => {
                self.timed = None;
                Some(now.saturating_sub(sent))
            }
            Some((sent, owed)) => {
                self.timed = Some((sent, owed.saturating_sub(1)));
                None
            }
            None => None,
        }
    }
}

/// What one voter's path measured: its round trips (RFC 9002 §5.3's smoothed estimator, which
/// follows a peer that became slow at once), from its heartbeats', pre-votes' and votes' answers.
/// None of them touches the voter's disk; the ballot adds the flush itself (`Ballot::measure`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Path {
    rtt: ExchangeRtt,
    beats: Exchange,
    pre_votes: Exchange,
    votes: Exchange,
}

/// The lateness of the leader's link's arrivals past their expected time (hyper-timing's
/// `Arrivals`): a follower's of its leader's heartbeats, a leader's of its followers' answers to
/// them. Each lateness is the gap since the same peer's previous arrival less the interval its
/// heartbeats are sent at. Kept for one term, the term's leader's link: a new term is a new link.
/// The mean and variance are Welford's running ones ("Note on a Method for Calculating Corrected
/// Sums of Squares and Products", Technometrics 1962), over every arrival the term saw.
#[derive(Clone, Debug, Default, PartialEq)]
struct Link {
    term: u64,
    last: BTreeMap<u64, u64>,
    count: u64,
    mean: f64,
    squares: f64,
}

impl Link {
    fn arrived(&mut self, term: u64, from: u64, now: u64, interval: u64) {
        if term != self.term {
            *self = Self {
                term,
                ..Self::default()
            };
        }
        if let Some(last) = self.last.insert(from, now) {
            let late = now.saturating_sub(last).saturating_sub(interval) as f64;
            self.count = self.count.saturating_add(1);
            let delta = late - self.mean;
            self.mean += delta / self.count as f64;
            self.squares += delta * (late - self.mean);
        }
    }
    /// What the term's arrivals measured: none before two arrivals from one peer. The share no
    /// lateness seen bounds is `1/(m + 1)` over `m` arrivals (Rényi 1962, as `Arrivals` states it);
    /// `E(D)` is zero, the sender's schedule not being on this clock.
    fn arrivals(&self) -> Option<Arrivals> {
        if self.count == 0 {
            return None;
        }
        // Format: nanoseconds a second, the unit the stamps are in.
        let seconds = |nanos: f64| Duration::try_from_secs_f64(nanos.max(0.0) / 1e9).ok();
        Some(Arrivals {
            unseen: 1.0 / (self.count as f64 + 1.0),
            lateness: seconds(self.mean)?,
            deviation: seconds((self.squares / self.count as f64).sqrt())?,
            mean_delay: Duration::ZERO,
        })
    }
    /// Forgets the arrivals of a member that left.
    fn retain(&mut self, voters: &[u64]) {
        self.last.retain(|peer, _| voters.contains(peer));
    }
}

/// What the owner measured, and the pace derived from it.
#[derive(Clone, Debug)]
pub(super) struct Pace {
    paths: BTreeMap<u64, Path>,
    /// The log's writes, submit to durable, on the owner's clock: the flush a vote waits for
    /// before a replica that has made no vote durable yet (hyper-durable's `Replica::measure`
    /// takes the node's log writes the same way).
    flushes: Flushes,
    /// The owner's timed waits' lateness: `G`, the granularity under every tail.
    lateness: Lateness,
    link: Link,
    /// The owner's clock reading when it opened: its exposure counts from there.
    opened: u64,
    current: TickPace,
}

impl Pace {
    /// A pace at hyper-timing's start state, `start` the period the owner opened with, at the
    /// owner's clock reading `now`.
    pub(super) fn new(start: Duration, now: u64) -> Self {
        Self {
            paths: BTreeMap::new(),
            flushes: Flushes::new(),
            // Format: the owner's stamps are whole nanoseconds, its clock's least step as read.
            lateness: Lateness::new(Duration::from_nanos(1)),
            link: Link::default(),
            opened: now,
            current: TickPace::floor(start),
        }
    }
    /// The period between ticks in force.
    pub(super) fn period(&self) -> Duration {
        self.current.period
    }
    /// The interval a leader's heartbeats leave at, in nanoseconds: `heartbeat_tick` periods.
    fn interval(&self, heartbeat_tick: usize) -> u64 {
        let ticks = u32::try_from(heartbeat_tick).unwrap_or(u32::MAX);
        u64::try_from(self.current.period.saturating_mul(ticks).as_nanos()).unwrap_or(u64::MAX)
    }
    /// The owner sent `message` at `now`. `lost` is the heartbeats unanswered at which the core
    /// takes a peer for lost: its election timeout's count of them.
    pub(super) fn sent(&mut self, message: &Message, now: u64, lost: u32) {
        let path = || Path::default();
        match message.msg_type {
            MessageType::MsgHeartbeat => self
                .paths
                .entry(message.to)
                .or_insert_with(path)
                .beats
                .sent(now, lost),
            MessageType::MsgRequestPreVote => self
                .paths
                .entry(message.to)
                .or_insert_with(path)
                .pre_votes
                .sent(now, 1),
            MessageType::MsgRequestVote => self
                .paths
                .entry(message.to)
                .or_insert_with(path)
                .votes
                .sent(now, 1),
            _ => {}
        }
    }
    /// The owner took `message` at `now`, its heartbeats leaving every `heartbeat_tick` ticks.
    pub(super) fn took(&mut self, message: &Message, now: u64, heartbeat_tick: usize) {
        let interval = self.interval(heartbeat_tick);
        let Some(path) = self.paths.get_mut(&message.from) else {
            if message.msg_type == MessageType::MsgHeartbeat {
                self.link.arrived(message.term, message.from, now, interval);
            }
            return;
        };
        let round_trip = match message.msg_type {
            MessageType::MsgHeartbeat => {
                self.link.arrived(message.term, message.from, now, interval);
                None
            }
            MessageType::MsgHeartbeatResponse => {
                self.link.arrived(message.term, message.from, now, interval);
                path.beats.answered(now)
            }
            MessageType::MsgRequestPreVoteResponse => path.pre_votes.answered(now),
            MessageType::MsgRequestVoteResponse => path.votes.answered(now),
            _ => None,
        };
        if let Some(round_trip) = round_trip {
            path.rtt.on_sample(round_trip);
        }
    }
    /// A write of the log, submitted at `submitted` and durable at `durable`.
    pub(super) fn flushed(&mut self, submitted: u64, durable: u64) {
        // A fold that is full keeps its mean: the sample is one of more than it counts.
        let _ = self.flushes.on_flush(submitted, durable);
    }
    /// A timed wait asked to end at `deadline` ended at `woke`.
    pub(super) fn waited(&mut self, deadline: u64, woke: u64) {
        // A fold that is full keeps its mean.
        let _ = self.lateness.on_wait(deadline, woke);
    }
    /// The pace from what is measured now, at the owner's clock reading `now`: for `local` among
    /// `voters`, a core of `election_tick` and `heartbeat_tick` ticks, and the mean flush of the
    /// replica's vote writes, if it made one durable. Kept as it was while a quorum's paths, the
    /// timer or the link is unmeasured: no pace is derived from what nobody measured.
    pub(super) fn derive(
        &mut self,
        voters: &[u64],
        local: u64,
        election_tick: usize,
        heartbeat_tick: usize,
        votes: Option<Duration>,
        now: u64,
    ) -> TickPace {
        self.paths.retain(|peer, _| voters.contains(peer));
        self.link.retain(voters);
        if let Some(pace) = self.measured(voters, local, election_tick, heartbeat_tick, votes, now)
        {
            self.current = pace;
        }
        self.current
    }
    fn measured(
        &self,
        voters: &[u64],
        local: u64,
        election_tick: usize,
        heartbeat_tick: usize,
        votes: Option<Duration>,
        now: u64,
    ) -> Option<TickPace> {
        let granularity = self.lateness.granularity()?;
        let durable = votes
            .or_else(|| self.flushes.mean())
            .unwrap_or(Duration::ZERO);
        let paths = voters
            .iter()
            .filter(|voter| **voter != local)
            .filter_map(|voter| self.paths.get(voter).map(|path| &path.rtt));
        let ballot = Ballot::measure(paths, voters.len(), durable, granularity)?;
        let span = ballot.span(granularity)?;
        // The owner's own exposure since it opened, with no failure seen: the Jeffreys MTBF,
        // `2T` (hyper-timing's `Exposure`).
        let mtbf = Exposure::of(Duration::from_nanos(now.saturating_sub(self.opened)), 0).mtbf()?;
        let costs = Costs {
            election: span.election,
            mtbf,
        };
        let interval = Duration::from_nanos(self.interval(heartbeat_tick));
        let detector = arrival_detector_at(&self.link.arrivals()?, &costs, granularity, interval)?;
        let timing = ElectionTiming::derive(self.current.period, &detector, &span, &ballot);
        // No period finer than the timer keeps; no ceiling picked above the measured election.
        Some(TickPace::derive(
            granularity,
            Duration::MAX,
            election_tick,
            &timing,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn message(msg_type: MessageType, from: u64, to: u64, term: u64) -> Message {
        Message {
            msg_type,
            from,
            to,
            term,
            ..Message::default()
        }
    }

    #[test]
    fn election_tick_an_answer_times_its_own_request() {
        let mut exchange = Exchange::default();
        exchange.sent(0, 10);
        exchange.sent(4 * MS, 10);
        // The first answer answers the first request, which was timed.
        assert_eq!(exchange.answered(3 * MS), Some(3 * MS));
        // The second request left while one was timed: not timed, nothing sampled.
        assert_eq!(exchange.answered(7 * MS), None);
        // One sent while two were unanswered is timed past both.
        exchange.sent(8 * MS, 10);
        exchange.sent(9 * MS, 10);
        exchange.sent(10 * MS, 10);
        let mut fresh = Exchange::default();
        fresh.sent(0, 10);
        fresh.sent(MS, 10);
        fresh.sent(2 * MS, 10);
        assert_eq!(fresh.answered(5 * MS), Some(5 * MS));
        assert_eq!(exchange.answered(11 * MS), Some(3 * MS));
    }

    #[test]
    fn election_tick_a_request_sent_behind_unanswered_ones_waits_for_their_answers() {
        let mut exchange = Exchange::default();
        exchange.sent(0, 10);
        assert_eq!(exchange.answered(MS), Some(MS));
        exchange.sent(2 * MS, 10);
        exchange.sent(3 * MS, 10);
        assert_eq!(exchange.answered(4 * MS), Some(2 * MS));
        // Now one is unanswered: the next timed request owes it an answer first.
        exchange.sent(5 * MS, 10);
        assert_eq!(
            exchange.answered(6 * MS),
            None,
            "the answer to the request at 3 ms"
        );
        assert_eq!(exchange.answered(8 * MS), Some(3 * MS));
    }

    #[test]
    fn election_tick_a_peer_the_core_takes_for_lost_starts_its_count_again() {
        let mut exchange = Exchange::default();
        // Two heartbeats an election timeout: two unanswered and the peer is lost.
        exchange.sent(0, 2);
        exchange.sent(MS, 2);
        exchange.sent(2 * MS, 2);
        assert_eq!(
            exchange.answered(3 * MS),
            Some(MS),
            "the request after the loss was timed"
        );
    }

    #[test]
    fn tick_the_pace_is_the_start_until_a_quorum_the_timer_and_the_link_are_measured() {
        let start = Duration::from_millis(10);
        let mut pace = Pace::new(start, 0);
        assert_eq!(
            pace.derive(&[1, 2, 3], 1, 20, 2, None, 50 * MS),
            TickPace::floor(start)
        );
        // The timer and a quorum's path measured, no link: still the start.
        pace.waited(MS, MS + 50_000);
        pace.sent(&message(MessageType::MsgRequestVote, 1, 2, 1), MS, 1);
        pace.took(
            &message(MessageType::MsgRequestVoteResponse, 2, 1, 1),
            2 * MS,
            2,
        );
        assert_eq!(
            pace.derive(&[1, 2, 3], 1, 20, 2, None, 50 * MS),
            TickPace::floor(start)
        );
    }

    /// A leader that measured its paths, its timer and its followers' answers to its heartbeats.
    fn measured(round_trip: u64, jitter: u64) -> Pace {
        let start = Duration::from_millis(10);
        let mut pace = Pace::new(start, 0);
        for wait in 0..8 {
            pace.waited(wait * MS, wait * MS + 50_000);
        }
        let mut now = 0;
        for beat in 0..64u64 {
            for peer in [2, 3] {
                pace.sent(&message(MessageType::MsgHeartbeat, 1, peer, 1), now, 10);
                let late = (beat * 7_919 + peer * 104_729) % jitter.max(1);
                pace.took(
                    &message(MessageType::MsgHeartbeatResponse, peer, 1, 1),
                    now + round_trip + late,
                    2,
                );
            }
            now += 2 * u64::try_from(pace.period().as_nanos()).unwrap();
        }
        pace.derive(&[1, 2, 3], 1, 20, 2, Some(Duration::from_micros(200)), now);
        pace
    }

    #[test]
    fn tick_the_pace_follows_the_measured_election() {
        let near = measured(MS / 2, MS / 10);
        let far = measured(80 * MS, 20 * MS);
        assert!(near.current.samples > 0 && far.current.samples > 0);
        // Its ticks cover the measured election, whichever period that takes.
        for pace in [&near, &far] {
            let timeout = pace.current.election_timeout(20);
            assert!(timeout >= pace.current.covered, "{:?}", pace.current);
        }
        // A slower group's paths and link take a longer period than a near one's.
        assert!(
            far.period() > near.period(),
            "{:?} vs {:?}",
            far.current,
            near.current
        );
        // Neither is bound by the period the owner opened with.
        assert_ne!(near.period(), Duration::from_millis(10));
        assert_ne!(far.period(), Duration::from_millis(10));
    }

    #[test]
    fn tick_a_new_term_measures_its_link_afresh_and_a_member_that_left_is_forgotten() {
        let mut pace = measured(MS, MS / 10);
        assert!(pace.link.count > 0);
        pace.took(&message(MessageType::MsgHeartbeat, 3, 1, 2), 10 * MS, 2);
        assert_eq!((pace.link.term, pace.link.count), (2, 0));
        pace.derive(&[1, 2], 1, 20, 2, None, 20 * MS);
        assert!(!pace.paths.contains_key(&3) && !pace.link.last.contains_key(&3));
    }
}
