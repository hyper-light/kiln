//! The network, under the nemesis: frames between nodes dropped, duplicated, delayed (so arriving
//! out of order) and cut off by partitions, while the nemesis lasts; after it, every frame
//! arrives, soon. A model asks what becomes of each frame it sends and puts its arrivals in its
//! own queue, by how the frame travels:
//!
//! - [`Net::send`]: a datagram, lost, doubled or overtaken;
//! - [`Net::send_in_order`]: on an ordered lane (a QUIC stream, a TCP connection): lost (a lane
//!   that sheds frames or a connection that drops), never doubled, never overtaking one sent
//!   before it on that lane;
//! - [`Net::send_once`]: a request or its answer on a connection of its own: lost or late, never
//!   doubled;
//! - [`Net::send_late`]: only ever late (an announcement a real link carries again until it's
//!   heard).

use std::collections::BTreeMap;
use std::time::Duration;

use crate::rng::Rng;

/// A node of a simulation: a machine, a process, a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Node(pub u32);

/// How hard the network's nemesis is, and for how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Faults {
    /// Frames lost, per thousand.
    pub drop: u64,
    /// Frames that arrive twice, per thousand.
    pub dup: u64,
    /// Longest a frame takes while the nemesis lasts (each draws its own: so they reorder).
    pub slow: Duration,
    /// Longest a frame takes after it.
    pub fast: Duration,
    /// When the nemesis stops: after it, nothing is lost, doubled or cut off.
    pub until: Duration,
}

impl Faults {
    /// A network that loses, doubles and cuts nothing, each frame within `fast`.
    pub fn none(fast: Duration) -> Self {
        Self { drop: 0, dup: 0, slow: fast, fast, until: Duration::ZERO }
    }

    /// A seed's own nemesis, lasting until `until`: up to 30% of frames lost and 30% doubled, each
    /// taking up to `slow` meanwhile (at least `fast`).
    pub fn draw(rng: &mut Rng, fast: Duration, slow: Duration, until: Duration) -> Self {
        let slow = rng.span(fast, slow.max(fast));
        Self { drop: rng.range(0, 300), dup: rng.range(0, 300), slow, fast, until }
    }
}

/// What the network did, counted: across seeds, a fault that never happened tested nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetCounts {
    pub sent: u64,
    pub dropped: u64,
    pub duplicated: u64,
    /// Frames sent over a cut link.
    pub cut: u64,
    /// Partitions made.
    pub partitions: u64,
}

impl NetCounts {
    pub fn add(&mut self, o: &Self) {
        self.sent = self.sent.saturating_add(o.sent);
        self.dropped = self.dropped.saturating_add(o.dropped);
        self.duplicated = self.duplicated.saturating_add(o.duplicated);
        self.cut = self.cut.saturating_add(o.cut);
        self.partitions = self.partitions.saturating_add(o.partitions);
    }
}

/// What becomes of a frame: when it arrives, if it does (twice, for one duplicated).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    Lost,
    Once(Duration),
    Twice(Duration, Duration),
}

impl Fate {
    /// Each arrival.
    pub fn arrivals(self) -> impl Iterator<Item = Duration> {
        let (a, b) = match self {
            Self::Lost => (None, None),
            Self::Once(t) => (Some(t), None),
            Self::Twice(t, u) => (Some(t), Some(u)),
        };
        a.into_iter().chain(b)
    }
}

/// The links between a simulation's nodes.
#[derive(Debug, Clone)]
pub struct Net {
    faults: Faults,
    rng: Rng,
    /// Links cut, each until when (both directions).
    cuts: BTreeMap<(Node, Node), Duration>,
    /// Each ordered lane's last arrival: a frame sent on it after arrives after it.
    lanes: BTreeMap<(Node, Node, u32), Duration>,
    counts: NetCounts,
}

/// The shortest a frame takes.
const MIN_DELAY: Duration = Duration::from_micros(100);

fn link(a: Node, b: Node) -> (Node, Node) {
    if a <= b { (a, b) } else { (b, a) }
}

impl Net {
    pub fn new(faults: Faults, rng: Rng) -> Self {
        Self { faults, rng, cuts: BTreeMap::new(), lanes: BTreeMap::new(), counts: NetCounts::default() }
    }

    pub fn faults(&self) -> Faults {
        self.faults
    }

    /// Whether the nemesis still acts at `now`.
    pub fn faulty(&self, now: Duration) -> bool {
        now < self.faults.until
    }

    /// Whether the link between `a` and `b` is cut at `now`.
    pub fn is_cut(&self, now: Duration, a: Node, b: Node) -> bool {
        self.faulty(now) && self.cuts.get(&link(a, b)).is_some_and(|until| *until > now)
    }

    /// Cuts the link between `a` and `b` until `until` (no later than the nemesis's end).
    pub fn cut(&mut self, a: Node, b: Node, until: Duration) {
        let until = until.min(self.faults.until);
        let slot = self.cuts.entry(link(a, b)).or_default();
        *slot = (*slot).max(until);
        self.counts.partitions = self.counts.partitions.saturating_add(1);
    }

    /// Cuts `a` off from every node of `others` until `until`.
    pub fn isolate(&mut self, a: Node, others: &[Node], until: Duration) {
        for b in others.iter().filter(|b| **b != a) {
            self.cut(a, *b, until);
        }
    }

    fn delay(&mut self, now: Duration) -> Duration {
        let most = if self.faulty(now) { self.faults.slow } else { self.faults.fast };
        now.saturating_add(self.rng.span(MIN_DELAY, most.max(MIN_DELAY)))
    }

    /// What becomes of a frame `from` sends `to` at `now`.
    pub fn send(&mut self, now: Duration, from: Node, to: Node) -> Fate {
        self.counts.sent = self.counts.sent.saturating_add(1);
        if self.faulty(now) {
            if self.is_cut(now, from, to) {
                self.counts.cut = self.counts.cut.saturating_add(1);
                return Fate::Lost;
            }
            if self.rng.chance(self.faults.drop) {
                self.counts.dropped = self.counts.dropped.saturating_add(1);
                return Fate::Lost;
            }
            if self.rng.chance(self.faults.dup) {
                self.counts.duplicated = self.counts.duplicated.saturating_add(1);
                let (a, b) = (self.delay(now), self.delay(now));
                return Fate::Twice(a, b);
            }
        }
        Fate::Once(self.delay(now))
    }

    /// Whether a frame sent now from `from` to `to` is lost (cut off, or dropped), counted.
    fn lost(&mut self, now: Duration, from: Node, to: Node) -> bool {
        self.counts.sent = self.counts.sent.saturating_add(1);
        if !self.faulty(now) {
            return false;
        }
        if self.is_cut(now, from, to) {
            self.counts.cut = self.counts.cut.saturating_add(1);
            return true;
        }
        if self.rng.chance(self.faults.drop) {
            self.counts.dropped = self.counts.dropped.saturating_add(1);
            return true;
        }
        false
    }

    /// What becomes of a frame `from` sends `to` at `now` on ordered lane `lane`: it arrives after
    /// every frame sent before it on that lane, or not at all; never twice.
    pub fn send_in_order(&mut self, now: Duration, from: Node, to: Node, lane: u32) -> Fate {
        if self.lost(now, from, to) {
            return Fate::Lost;
        }
        let drawn = self.delay(now);
        let last = self.lanes.entry((from, to, lane)).or_default();
        let at = drawn.max(last.saturating_add(Duration::from_nanos(1)));
        *last = at;
        Fate::Once(at)
    }

    /// What becomes of a request (or its answer) `from` sends `to` at `now`, on a connection of
    /// its own: lost or late, never twice.
    pub fn send_once(&mut self, now: Duration, from: Node, to: Node) -> Fate {
        if self.lost(now, from, to) {
            return Fate::Lost;
        }
        Fate::Once(self.delay(now))
    }

    /// When a frame that's only ever late arrives: never lost or doubled, but delayed (past a cut,
    /// until the cut heals).
    pub fn send_late(&mut self, now: Duration, from: Node, to: Node) -> Duration {
        self.counts.sent = self.counts.sent.saturating_add(1);
        let healed = if self.is_cut(now, from, to) { self.cuts.get(&link(from, to)).copied().unwrap_or(now) } else { now };
        self.delay(healed.max(now))
    }

    pub fn counts(&self) -> NetCounts {
        self.counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CP-T6.1: while the nemesis lasts, frames are lost, doubled, cut off and reordered; after
    /// it, every frame arrives once, soon; and a seed's network does the same every time.
    #[test]
    fn the_nemesis_loses_doubles_cuts_and_reorders_until_it_stops() {
        let ms = Duration::from_millis;
        let faults = Faults { drop: 200, dup: 200, slow: ms(50), fast: ms(2), until: ms(1000) };
        let run = |seed| {
            let mut net = Net::new(faults, Rng::new(seed));
            let (a, b, c) = (Node(0), Node(1), Node(2));
            net.cut(a, c, ms(500));
            let mut arrivals = Vec::new();
            for i in 0..1000u64 {
                let now = ms(i);
                arrivals.extend(net.send(now, a, b).arrivals().map(|t| (t, i)));
                assert!(i >= 500 || net.send(now, a, c) == Fate::Lost, "a cut link carries nothing");
            }
            (net.counts(), arrivals)
        };
        let (counts, mut arrivals) = run(3);
        assert_eq!((counts, arrivals.clone()), run(3), "a seed's network is the same every time");
        assert!(counts.dropped > 0 && counts.duplicated > 0 && counts.cut > 0 && counts.partitions == 1, "{counts:?}");
        arrivals.sort();
        let sent_order: Vec<u64> = arrivals.iter().map(|(_, i)| *i).collect();
        assert!(sent_order.windows(2).any(|w| matches!(w, [x, y] if x > y)), "some frames overtook others");
        let mut calm = Net::new(faults, Rng::new(3));
        for i in 1000..2000u64 {
            let now = ms(i);
            let fate = calm.send(now, Node(0), Node(1));
            assert!(matches!(fate, Fate::Once(t) if t > now && t <= now + ms(2)), "after the nemesis, a frame arrives once, soon: {fate:?}");
        }
    }

    /// CP-T6.1: on an ordered lane, frames are lost but never overtake one another or come twice;
    /// a request is lost or late, never doubled.
    #[test]
    fn an_ordered_lane_loses_frames_but_never_reorders_or_doubles_them() {
        let ms = Duration::from_millis;
        let mut net = Net::new(Faults { drop: 200, dup: 500, slow: ms(80), fast: ms(2), until: ms(1000) }, Rng::new(11));
        let mut last = Duration::ZERO;
        let mut lost = 0;
        for i in 0..1000u64 {
            let fate = net.send_in_order(ms(i), Node(0), Node(1), 7);
            assert!(!matches!(fate, Fate::Twice(..)), "an ordered lane doubled a frame");
            match fate {
                Fate::Once(t) => {
                    assert!(t > last, "a frame overtook one sent before it");
                    last = t;
                }
                _ => lost += 1,
            }
            assert!(!matches!(net.send_once(ms(i), Node(0), Node(1)), Fate::Twice(..)));
        }
        assert!(lost > 0);
    }

    /// CP-T6.1: a frame that's only ever late arrives once the cut it met heals.
    #[test]
    fn a_late_frame_waits_out_its_cut() {
        let ms = Duration::from_millis;
        let mut net = Net::new(Faults { drop: 1000, dup: 0, slow: ms(5), fast: ms(1), until: ms(100) }, Rng::new(1));
        net.cut(Node(0), Node(1), ms(80));
        let t = net.send_late(ms(10), Node(0), Node(1));
        assert!(t > ms(80) && t <= ms(85), "{t:?}");
    }
}
