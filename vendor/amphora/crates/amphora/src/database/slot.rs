//! The leader's write slot (replication.md, "A follower's write under steady leader writes: the
//! write slot"; Amphora goal 9c63f7d4, AC-T1.8.W1). A prepared write applies only on the entry
//! before it (`replication::on_base`), and a follower's reaches the leader's log a ReadIndex round,
//! its preparation and a forward after the leader's own would: under steady leader writes it is
//! refused round after round. Once one of a member's writes is refused at the leader's append,
//! that member is queued, and its next barrier (its ReadIndex) waits at the leader for its slot.
//! While it holds the slot, the leader appends nothing of its own and holds the other members'
//! forwards, until the member's next forward arrives or the slot's learned length passes. A member
//! that never comes back never holds the leader. A pure core: the owner tells it what the leader
//! appends, receives and measures, and asks it what to hold and what to step.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::meter::SlotEnd;

/// What a slot's length is learned against: the core's election timeout, a member's slot before
/// any of its retries is measured. A learned gap is believed for one to two of its member's
/// generations, each as long as the member went between its last two learned gaps, the first
/// counted from its first slot (replication.md, "Decision (lane constants): generations learned
/// where they run"). No ceiling bounds a gap by a number: a member is gone, not slow, when the
/// host's detector suspects it while its retry is awaited (`keep`), a verdict learned where it
/// runs (Ada's word, 2026-11-21: a value measured once on one host and frozen is picked). So the
/// slot adds no number.
#[derive(Clone, Copy, Debug)]
pub(super) struct Bounds {
    pub(super) election: Duration,
}

/// A retry's gap the slot does not learn, and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum Unmeasured {
    /// The host's detector suspected the member while its retry was awaited: gone, not slow.
    #[error(
        "member {member} forwarded {gap:?} after its write slot began, suspected by the host's detector meanwhile"
    )]
    Suspected { member: u64, gap: Duration },
}

/// A slot that ended: whose, how, and how long the leader held its own writes for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Ended {
    pub(super) member: u64,
    pub(super) how: SlotEnd,
    pub(super) held: Duration,
}

/// What a message does: the message to step now, if any (before anything `ready` gives), the slot
/// it ended, and a gap it measured but did not learn.
pub(super) struct Taken<T> {
    pub(super) step: Option<T>,
    pub(super) ended: Option<Ended>,
    pub(super) unmeasured: Option<Unmeasured>,
}

struct Holding {
    member: u64,
    since: Instant,
    until: Instant,
    // The leader's last index as the slot began: the holder's barrier covers it.
    last: u64,
}

/// One member's retries as the leader measured them: the longest gap from a slot's start to its
/// next forward in this generation and in the one before, and the start of the slot it is
/// waiting on, if any. A slot that expired keeps waiting, so a late retry still teaches the next,
/// unless the host's detector suspected the member while it waited (`suspected`).
struct Gap {
    member: u64,
    current: Option<Duration>,
    before: Option<Duration>,
    from: Option<Instant>,
    suspected: bool,
    /// The start of its generation; its last learned gap's end (its first slot's start before
    /// one); and how long a generation lasts: the time between its last two learned gaps.
    began: Instant,
    learned: Instant,
    generation: Option<Duration>,
}

/// One leader's slots, in the term it leads. The queue, the barriers waiting, the gaps and the
/// forwards held are each at most one a member of the configuration (the owner keeps them to its
/// members, `keep`), so at most `MEMBERS`; `ready` holds what an end released until the owner's
/// same step drains it.
pub(super) struct Slots<T> {
    bounds: Bounds,
    term: Option<u64>,
    // Members a refusal queued, whose next barrier has not come yet.
    queue: VecDeque<u64>,
    // Queued members' barriers, in arrival order, each waiting for its member's slot.
    waiting: VecDeque<(u64, T)>,
    holding: Option<Holding>,
    // The leader's own turn is owed: one preparation of its own between two slots.
    turn: bool,
    forwards: VecDeque<(u64, T)>,
    // The holder's barrier, until the leader's commit reaches the slot's start.
    read: Option<(u64, T)>,
    ready: VecDeque<(u64, T)>,
    gaps: Vec<Gap>,
}

impl<T> Slots<T> {
    /// The bounds as the owner's pace moved: a slot begun after takes them.
    pub(super) fn bound(&mut self, bounds: Bounds) {
        self.bounds = bounds;
    }
    pub(super) fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            term: None,
            queue: VecDeque::new(),
            waiting: VecDeque::new(),
            holding: None,
            turn: false,
            forwards: VecDeque::new(),
            read: None,
            ready: VecDeque::new(),
            gaps: Vec::new(),
        }
    }

    /// The term this member leads at `at`, or none. A change ends a slot held in another term as
    /// released, its holds and the barriers waiting ready to step as they would have been had
    /// they arrived then, and forgets the queue: slots belong to one term at one leader.
    pub(super) fn lead(&mut self, term: Option<u64>, at: Instant) -> Option<Ended> {
        if term == self.term {
            return None;
        }
        self.term = term;
        let ended = self.end(at, SlotEnd::Released);
        self.ready.extend(self.waiting.drain(..));
        self.queue.clear();
        self.turn = false;
        for gap in &mut self.gaps {
            gap.from = None;
        }
        ended
    }

    /// Keeps the members `member` says are in the configuration and `suspected` doesn't: one
    /// that left or that the host's detector suspects leaves the queue, its waiting barrier steps,
    /// and a slot held for it ends released. One that left is forgotten.
    pub(super) fn keep(
        &mut self,
        at: Instant,
        member: impl Fn(u64) -> bool,
        suspected: impl Fn(u64) -> bool,
    ) -> Option<Ended> {
        let kept = |queued: u64| member(queued) && !suspected(queued);
        self.queue.retain(|queued| kept(*queued));
        let mut index = 0;
        while let Some((queued, _)) = self.waiting.get(index) {
            if kept(*queued) {
                index = index.saturating_add(1);
            } else if let Some(gone) = self.waiting.remove(index) {
                self.ready.push_back(gone);
            }
        }
        self.gaps.retain(|gap| member(gap.member));
        for gap in &mut self.gaps {
            if gap.from.is_some() && suspected(gap.member) {
                gap.suspected = true;
            }
        }
        if self
            .holding
            .as_ref()
            .is_some_and(|holding| !kept(holding.member))
        {
            return self.end(at, SlotEnd::Released);
        }
        None
    }

    /// One of `member`'s writes was refused at the leader's append: it joins the queue's tail,
    /// unless it is queued already.
    pub(super) fn refused(&mut self, member: u64) {
        if self.term.is_some() && !self.queue.contains(&member) {
            self.queue.push_back(member);
        }
    }

    /// `member`'s barrier (its ReadIndex) at `at`. A queued member's waits for its slot. The
    /// holder's second barrier before any forward means its slot's work was no write (a read, or
    /// a write that prepared nothing), so its slot ends released and the barrier steps. Anything
    /// else steps now.
    pub(super) fn read(&mut self, member: u64, at: Instant, message: T) -> Taken<T> {
        if self.term.is_none() {
            return Taken {
                step: Some(message),
                ended: None,
                unmeasured: None,
            };
        }
        if let Some(position) = self.queue.iter().position(|queued| *queued == member) {
            self.queue.remove(position);
            // A barrier from a member whose last still waits is a new operation (its owner
            // makes one at a time): the older steps, the newer waits in its place.
            if let Some(position) = self.waiting.iter().position(|(m, _)| *m == member)
                && let Some(older) = self.waiting.remove(position)
            {
                self.ready.push_back(older);
            }
            self.waiting.push_back((member, message));
            return Taken {
                step: None,
                ended: None,
                unmeasured: None,
            };
        }
        let ended = if self
            .holding
            .as_ref()
            .is_some_and(|holding| holding.member == member)
        {
            if let Some(gap) = self.gaps.iter_mut().find(|gap| gap.member == member) {
                gap.from = None;
            }
            self.end(at, SlotEnd::Released)
        } else {
            None
        };
        Taken {
            step: Some(message),
            ended,
            unmeasured: None,
        }
    }

    /// Starts the slot of the first member whose barrier waits, at `at`, the leader's last index
    /// being `last` and its commit `commit`, unless a slot is held, or the leader's own turn is
    /// owed and `own` (it has a write to prepare). The barrier steps once the commit reaches
    /// `last`, so the member's retry prepares on every entry before the slot (amphora-seam-owner-
    /// 2's ruling, Q2 b). Returns whose slot began and its length.
    pub(super) fn start(
        &mut self,
        at: Instant,
        last: u64,
        commit: u64,
        own: bool,
    ) -> Option<(u64, Duration)> {
        if self.term.is_none() || self.holding.is_some() {
            return None;
        }
        if self.turn {
            if own {
                return None;
            }
            self.took_turn();
        }
        let (member, barrier) = self.waiting.pop_front()?;
        let length = self.length(member);
        let Some(until) = at.checked_add(length) else {
            self.waiting.push_front((member, barrier));
            return None;
        };
        self.holding = Some(Holding {
            member,
            since: at,
            until,
            last,
        });
        if commit >= last {
            self.ready.push_back((member, barrier));
        } else {
            self.read = Some((member, barrier));
        }
        match self.gaps.iter_mut().find(|gap| gap.member == member) {
            Some(gap) => {
                gap.from = Some(at);
                gap.suspected = false;
            }
            None => self.gaps.push(Gap {
                member,
                current: None,
                before: None,
                from: Some(at),
                suspected: false,
                began: at,
                learned: at,
                generation: None,
            }),
        }
        Some((member, length))
    }

    /// The length of `member`'s next slot: the longest gap it took to retry in this generation
    /// and the one before, or the election timeout while none is measured.
    pub(super) fn length(&self, member: u64) -> Duration {
        self.gaps
            .iter()
            .find(|gap| gap.member == member)
            .and_then(|gap| match (gap.current, gap.before) {
                (None, None) => None,
                (current, before) => {
                    Some(current.unwrap_or_default().max(before.unwrap_or_default()))
                }
            })
            .unwrap_or(self.bounds.election)
    }

    /// Whether the leader appends nothing of its own now.
    pub(super) fn holds(&self) -> bool {
        self.holding.is_some()
    }

    /// Whether the leader's own turn is owed: a slot ended, and the leader has not written since.
    pub(super) fn turn(&self) -> bool {
        self.turn
    }

    /// The leader appended a write of its own, or has none to make: its turn is taken, and the
    /// forwards held through it are ready.
    pub(super) fn took_turn(&mut self) {
        self.turn = false;
        self.ready.extend(self.forwards.drain(..));
    }

    /// The leader ends the slot it holds at `at`, released, and lets the barriers waiting step and
    /// the queue go: a change of members or a hand-off of its lead waits on no member's retry.
    pub(super) fn release(&mut self, at: Instant) -> Option<Ended> {
        let ended = self.end(at, SlotEnd::Released);
        self.took_turn();
        self.ready.extend(self.waiting.drain(..));
        self.queue.clear();
        ended
    }

    /// At `at`: each member's generation of learned gaps ages once it has lasted as long as the
    /// member's last two learned gaps were apart, and a slot past its length ends expired.
    pub(super) fn expire(&mut self, at: Instant) -> Option<Ended> {
        for gap in &mut self.gaps {
            if gap
                .generation
                .is_some_and(|length| at.saturating_duration_since(gap.began) >= length)
            {
                gap.began = at;
                gap.before = gap.current.take();
            }
        }
        if self
            .holding
            .as_ref()
            .is_some_and(|holding| at > holding.until)
        {
            return self.end(at, SlotEnd::Expired);
        }
        None
    }

    /// A forward from `member` at `at`, past its sender's checks. The holder's is stepped and ends
    /// its slot; another member's is held while a slot is, and through the leader's turn after
    /// it, so the leader's write in its turn follows the holder's entry and nothing else
    /// (amphora-owner-t1-g-slot-878285a-20261111). A second from a member already held is a new
    /// write (the first's caller was answered: each owner forwards one write at a time), and
    /// stepping the first would append an entry before the holder's retry, or the leader's turn,
    /// and refuse it, so the slot or the turn ends, and both are ready, in order.
    pub(super) fn forward(&mut self, member: u64, at: Instant, message: T) -> Taken<T> {
        let unmeasured = self.learn(member, at).err();
        let Some(holder) = self.holding.as_ref().map(|holding| holding.member) else {
            if !self.turn {
                return Taken {
                    step: Some(message),
                    ended: None,
                    unmeasured,
                };
            }
            let again = self.forwards.iter().any(|(held, _)| *held == member);
            self.forwards.push_back((member, message));
            if again {
                self.took_turn();
            }
            return Taken {
                step: None,
                ended: None,
                unmeasured,
            };
        };
        if holder == member {
            let ended = self.end(at, SlotEnd::Forwarded);
            return Taken {
                step: Some(message),
                ended,
                unmeasured,
            };
        }
        let again = self.forwards.iter().any(|(held, _)| *held == member);
        self.forwards.push_back((member, message));
        let ended = if again {
            self.end(at, SlotEnd::Released)
        } else {
            None
        };
        Taken {
            step: None,
            ended,
            unmeasured,
        }
    }

    /// The leader's commit is `commit`: a held barrier it reaches the slot's start for is ready.
    pub(super) fn committed(&mut self, commit: u64) {
        if self
            .holding
            .as_ref()
            .is_some_and(|holding| commit >= holding.last)
            && let Some(read) = self.read.take()
        {
            self.ready.push_back(read);
        }
    }

    /// When the slot held ends by its length, if one is held: one of the owner's deadlines.
    pub(super) fn until(&self) -> Option<Instant> {
        self.holding.as_ref().map(|holding| holding.until)
    }

    /// Whether the holder's barrier is held: a test's view.
    #[cfg(test)]
    pub(super) fn holds_read(&self) -> bool {
        self.read.is_some()
    }

    /// The next held message an end released, with its sender, in the order they arrived.
    pub(super) fn ready(&mut self) -> Option<(u64, T)> {
        self.ready.pop_front()
    }

    fn learn(&mut self, member: u64, at: Instant) -> Result<(), Unmeasured> {
        let Some(gap) = self.gaps.iter_mut().find(|gap| gap.member == member) else {
            return Ok(());
        };
        let Some(from) = gap.from.take() else {
            return Ok(());
        };
        let measured = at.saturating_duration_since(from);
        if std::mem::take(&mut gap.suspected) {
            return Err(Unmeasured::Suspected {
                member,
                gap: measured,
            });
        }
        gap.current = Some(
            gap.current
                .map_or(measured, |longest| longest.max(measured)),
        );
        if at > gap.learned {
            gap.generation = Some(at.saturating_duration_since(gap.learned));
        }
        gap.learned = at;
        Ok(())
    }

    fn end(&mut self, at: Instant, how: SlotEnd) -> Option<Ended> {
        if let Some(read) = self.read.take() {
            self.ready.push_back(read);
        }
        self.ready.extend(self.forwards.drain(..));
        let holding = self.holding.take()?;
        self.turn = true;
        Some(Ended {
            member: holding.member,
            how,
            held: at.saturating_duration_since(holding.since),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);
    const BOUNDS: Bounds = Bounds {
        election: Duration::from_millis(200),
    };

    fn leading(at: Instant) -> Slots<&'static str> {
        let mut slots = Slots::new(BOUNDS);
        assert_eq!(slots.lead(Some(7), at), None);
        slots
    }
    fn drained(slots: &mut Slots<&'static str>) -> Vec<(u64, &'static str)> {
        std::iter::from_fn(|| slots.ready()).collect()
    }
    /// `member`'s barrier, queued: it waits.
    fn waits(slots: &mut Slots<&'static str>, member: u64, at: Instant) {
        assert!(slots.read(member, at, "barrier").step.is_none());
    }

    /// T-T1.8.W4 (AC-T1.8.W1): do refuse member 2, then take its barrier; expect it waiting, its
    /// slot begun with it, the election timeout long while nothing is measured, the barrier ready
    /// at once (the commit is at the leader's last index), another member's forward held, the
    /// holder's stepped first and ending its slot forwarded, then the held one ready.
    #[test]
    fn a_refused_members_barrier_begins_its_slot_and_its_forward_goes_first() {
        let t = Instant::now();
        let mut slots = leading(t);
        slots.refused(2);
        assert!(!slots.holds(), "no slot before the member comes back");
        waits(&mut slots, 2, t);
        assert_eq!(slots.start(t, 10, 10, false), Some((2, BOUNDS.election)));
        assert!(slots.holds());
        assert_eq!(drained(&mut slots), vec![(2, "barrier")]);
        let held = slots.forward(3, t + MS, "three");
        assert!(held.step.is_none() && held.ended.is_none());
        let retry = slots.forward(2, t + 5 * MS, "two");
        assert_eq!(retry.step, Some("two"));
        assert_eq!(
            retry.ended,
            Some(Ended {
                member: 2,
                how: SlotEnd::Forwarded,
                held: 5 * MS
            })
        );
        assert!(!slots.holds());
        assert_eq!(drained(&mut slots), vec![(3, "three")]);
        assert_eq!(slots.length(2), 5 * MS);
    }

    /// T-T1.8.W4: do refuse a member that never comes back; expect no slot, and another member's
    /// barrier and forward step at once.
    #[test]
    fn a_member_that_never_comes_back_never_holds_the_leader() {
        let t = Instant::now();
        let mut slots = leading(t);
        slots.refused(2);
        assert_eq!(slots.start(t, 1, 1, false), None);
        assert_eq!(slots.read(3, t, "other").step, Some("other"));
        assert_eq!(slots.forward(3, t, "write").step, Some("write"));
    }

    /// T-T1.8.W4: do refuse members 2 and 3 and take both barriers; expect one slot each, in the
    /// order the barriers came, and between them the leader's own turn while it has a write
    /// (`own`), none when it hasn't.
    #[test]
    fn slots_go_in_barrier_order_with_the_leaders_turn_between() {
        let t = Instant::now();
        let mut slots = leading(t);
        slots.refused(3);
        slots.refused(2);
        waits(&mut slots, 2, t);
        waits(&mut slots, 3, t);
        assert_eq!(slots.start(t, 1, 1, true).map(|(m, _)| m), Some(2));
        slots.forward(2, t, "two");
        assert_eq!(
            slots.start(t, 2, 2, true),
            None,
            "the leader's turn is owed"
        );
        slots.took_turn();
        assert_eq!(slots.start(t, 3, 3, true).map(|(m, _)| m), Some(3));
        slots.forward(3, t, "three");
        slots.refused(2);
        waits(&mut slots, 2, t);
        assert_eq!(
            slots.start(t, 4, 4, false).map(|(m, _)| m),
            Some(2),
            "no own write: no wait"
        );
        slots.forward(2, t, "two");
        assert_eq!(slots.start(t, 5, 5, false), None, "nothing waits");
    }

    /// T-T1.8.W4 (amphora-owner-t1-g-slot-878285a-20261111): do end member 2's slot, then forward
    /// from member 3 in the leader's turn; expect 3's forward held through the turn, ready once
    /// the leader's own write is appended, and held again only by a slot; and a turn the leader
    /// has no write for given up at the next start, its holds ready.
    #[test]
    fn forwards_wait_through_the_leaders_turn() {
        let t = Instant::now();
        let mut slots = leading(t);
        slots.refused(2);
        waits(&mut slots, 2, t);
        slots.start(t, 1, 1, false);
        drained(&mut slots);
        slots.forward(2, t, "two");
        assert!(slots.turn());
        assert!(
            slots.forward(3, t, "three").step.is_none(),
            "held through the turn"
        );
        assert!(drained(&mut slots).is_empty());
        slots.took_turn();
        assert_eq!(drained(&mut slots), vec![(3, "three")]);
        assert_eq!(slots.forward(3, t, "free").step, Some("free"));

        slots.refused(2);
        waits(&mut slots, 2, t);
        slots.start(t, 2, 2, false);
        drained(&mut slots);
        slots.forward(2, t, "two");
        slots.forward(3, t, "three");
        assert_eq!(slots.start(t, 3, 3, false), None, "nothing waits");
        assert!(!slots.turn(), "a turn with no write is given up");
        assert_eq!(drained(&mut slots), vec![(3, "three")]);
    }

    /// T-T1.8.W4 (amphora-seam-owner-2, Q1 a): do forward twice from member 3 during member 2's
    /// slot; expect the slot ended released, and both of 3's forwards ready in arrival order.
    #[test]
    fn a_second_forward_from_a_held_member_releases_the_slot() {
        let t = Instant::now();
        let mut slots = leading(t);
        slots.refused(2);
        waits(&mut slots, 2, t);
        slots.start(t, 1, 1, false);
        drained(&mut slots);
        assert!(slots.forward(3, t, "first").ended.is_none());
        let second = slots.forward(3, t + MS, "second");
        assert!(second.step.is_none());
        assert_eq!(
            second.ended.map(|e| (e.member, e.how)),
            Some((2, SlotEnd::Released))
        );
        assert_eq!(drained(&mut slots), vec![(3, "first"), (3, "second")]);
    }

    /// T-T1.8.W4: do take the holder's second barrier before any forward (its slot's work was a
    /// read); expect the slot ended released, the barrier stepped, and no gap learned.
    #[test]
    fn a_holders_second_barrier_releases_its_slot() {
        let t = Instant::now();
        let mut slots = leading(t);
        slots.refused(2);
        waits(&mut slots, 2, t);
        slots.start(t, 1, 1, false);
        drained(&mut slots);
        let again = slots.read(2, t + 3 * MS, "next");
        assert_eq!(again.step, Some("next"));
        assert_eq!(
            again.ended.map(|e| (e.member, e.how)),
            Some((2, SlotEnd::Released))
        );
        slots.forward(2, t + 9 * MS, "write");
        assert_eq!(slots.length(2), BOUNDS.election);
    }

    /// T-T1.8.W4 (Q3): do let a slot pass its length, then the member's retry arrive late; expect
    /// it ended expired only once past its length, the late retry held through the leader's turn
    /// but learned (no censoring), and the next slot as long as that retry took.
    #[test]
    fn an_expired_slot_teaches_the_next_its_late_retry() {
        let t = Instant::now();
        let mut slots = leading(t);
        slots.refused(2);
        waits(&mut slots, 2, t);
        slots.start(t, 1, 1, false);
        assert_eq!(
            slots.expire(t + BOUNDS.election),
            None,
            "at its length it still holds"
        );
        let ended = slots.expire(t + BOUNDS.election + MS).unwrap();
        assert_eq!(ended.how, SlotEnd::Expired);
        let late = slots.forward(2, t + 300 * MS, "late");
        assert!(
            late.step.is_none(),
            "held through the leader's turn after the slot"
        );
        assert_eq!(slots.length(2), 300 * MS);
    }

    /// T-T1.8.W4 (Q3): do let the host's detector suspect a member while its retry is awaited,
    /// then trust it again, and the retry come; expect the gap refused, typed, and not learned
    /// (the next slot keeps the election timeout), and a later retry no detector suspected
    /// through learned however long it took: no number bounds it.
    #[test]
    fn a_retry_its_member_was_suspected_through_is_not_learned() {
        let t = Instant::now();
        let mut slots = leading(t);
        slots.refused(2);
        waits(&mut slots, 2, t);
        slots.start(t, 1, 1, false);
        let ended = slots.keep(t + MS, |_| true, |member| member == 2);
        assert_eq!(
            ended.map(|e| (e.member, e.how)),
            Some((2, SlotEnd::Released))
        );
        slots.keep(t + 2 * MS, |_| true, |_| false);
        let late = t + 300 * MS;
        let forward = slots.forward(2, late, "gone");
        assert_eq!(
            forward.unmeasured,
            Some(Unmeasured::Suspected {
                member: 2,
                gap: 300 * MS,
            })
        );
        assert_eq!(slots.length(2), BOUNDS.election);
        drained(&mut slots);
        slots.took_turn();
        let at = t + 400 * MS;
        slots.refused(2);
        waits(&mut slots, 2, at);
        slots.start(at, 1, 1, false);
        slots.expire(at + 5 * BOUNDS.election);
        let slow = slots.forward(2, at + 5 * BOUNDS.election, "slow");
        assert_eq!(slow.unmeasured, None);
        assert_eq!(slots.length(2), 5 * BOUNDS.election);
    }

    /// T-T1.8.W4 (Q3), T-GN.1.2: do learn a gap 40 ms after the member's first slot began, then
    /// let time pass with none; expect it believed through the next generation, as long as the
    /// member took to its first gap (40 ms), and forgotten after it, back to the election timeout.
    #[test]
    fn a_learned_gap_is_believed_for_one_to_two_of_its_members_generations() {
        let t = Instant::now();
        let mut slots = leading(t);
        slots.expire(t);
        slots.refused(2);
        waits(&mut slots, 2, t);
        slots.start(t, 1, 1, false);
        slots.forward(2, t + 40 * MS, "two");
        assert_eq!(slots.length(2), 40 * MS);
        slots.expire(t + 40 * MS);
        assert_eq!(slots.length(2), 40 * MS, "believed in the next generation");
        slots.expire(t + 79 * MS);
        assert_eq!(slots.length(2), 40 * MS, "believed through it");
        slots.expire(t + 80 * MS);
        assert_eq!(slots.length(2), BOUNDS.election, "forgotten after it");
    }

    /// T-T1.8.W4 (Q1, point 1): do lose the lead, or remove or suspect the holder or a member
    /// whose barrier waits; expect each slot ended released, its holds and the waiting barriers
    /// ready, and the queue forgotten with the term.
    #[test]
    fn a_slot_dies_with_its_term_and_its_member() {
        let t = Instant::now();
        let mut slots = leading(t);
        slots.refused(2);
        slots.refused(3);
        waits(&mut slots, 2, t);
        waits(&mut slots, 3, t);
        slots.start(t, 1, 1, false);
        drained(&mut slots);
        slots.forward(4, t, "four");
        let ended = slots.lead(None, t + MS).unwrap();
        assert_eq!((ended.member, ended.how), (2, SlotEnd::Released));
        assert_eq!(drained(&mut slots), vec![(4, "four"), (3, "barrier")]);
        assert_eq!(
            slots.read(2, t, "free").step,
            Some("free"),
            "no slot without a term"
        );
        slots.lead(Some(8), t);
        assert_eq!(
            slots.read(2, t, "free").step,
            Some("free"),
            "the queue went with the term"
        );

        slots.refused(2);
        slots.refused(3);
        waits(&mut slots, 2, t);
        waits(&mut slots, 3, t);
        slots.start(t, 3, 3, false);
        drained(&mut slots);
        let removed = slots.keep(t, |m| m != 2, |_| false).unwrap();
        assert_eq!((removed.member, removed.how), (2, SlotEnd::Released));
        let suspected = slots.keep(t, |_| true, |m| m == 3);
        assert_eq!(suspected, None, "3 held no slot");
        assert_eq!(
            drained(&mut slots),
            vec![(3, "barrier")],
            "its barrier steps"
        );
        slots.took_turn();
        assert_eq!(slots.start(t, 4, 4, false), None, "nothing waits");
    }

    /// T-T1.8.W3's core (Q2 b): do begin a slot while the leader's commit is short of its last
    /// index; expect the holder's barrier held, and ready once the commit gets there.
    #[test]
    fn the_holders_barrier_waits_for_the_commit_to_reach_the_slots_start() {
        let t = Instant::now();
        let mut slots = leading(t);
        slots.refused(2);
        waits(&mut slots, 2, t);
        slots.start(t, 10, 9, false);
        assert!(slots.holds_read());
        slots.committed(9);
        assert!(drained(&mut slots).is_empty());
        slots.committed(10);
        assert_eq!(drained(&mut slots), vec![(2, "barrier")]);
    }
}
