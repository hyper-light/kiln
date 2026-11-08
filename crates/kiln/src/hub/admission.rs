//! Admission of runners reconnecting (docs/resilience.md R5; Ada, 2026-11-07). A server that starts again finds its
//! runners dialing it within a fraction of a second of its first answer (they learn when to dial from their own dials:
//! `crate::agent::redial`). Each reconnect costs the server its Hello and its catch-up, so it takes runners back only as
//! fast as it can: one over its limit is told its turn, and its turn is kept for it; and on its way down the server tells
//! its runners when it expects to answer again. No figure here is picked: each value is learned from what the server
//! measures, or derived from those, the derivation beside it.
//!
//! - **An admission** runs from a runner's `Admit`, the first thing it says on a connection, to its `Settled`, which it
//!   says once what it sends on its `Welcome` is out (what it held, its jobs' events and results, its terminals, its
//!   sessions' inventory and first updates). A runner that says neither, one from before this, is taken as before.
//! - **The control loop's work** on an admission ([`Admission::worked`]): the time the hub's control loop held the
//!   runner's own events, Admit to Settled, over the share of the loop left to admissions while it was in progress (one
//!   less the share the loop spent on other runners' messages meanwhile). The control loop takes each runner's Hello
//!   and control messages one at a time, waiting on the store as it does: it's the stage every admission passes in
//!   single file, so it finishes one admission per `work` at most. How busy its shard and the store are is in the
//!   time it holds an event, and the other runners' share is what it can't spare.
//! - **Letting go** of a runner over the limit costs the control loop its Admit and the frames its connection sends
//!   before it closes, read no further than their heads: learned as a mean over the runners let go, and added to the
//!   line as each is refused, since that time is the loop's before any turn after it.
//! - **An admission alone**: Admit to Settled with no other admission begun before it ended, or in progress as it
//!   began: the control loop's work and the rest (the runner's round trip, its catch-up on the session loops) with
//!   nothing ahead of it.
//! - **The limit**, by Little's law (admissions in progress = the rate they finish × the time each is in progress): at
//!   the control loop's rate, one per `work`, admissions each `alone` long overlap `alone / work` at once, and one more
//!   so the control loop never waits for the next (the rule below, which reckons the spread in). With nothing learned,
//!   one: the next when the last is done.
//! - **A turn** is `work` long: the control loop's time for one admission. A runner over the limit, or coming while
//!   turns given are still to come, is placed after every admission ahead of it, those in progress (a turn each: they
//!   may have begun together), the turns already given and what letting it go costs, and dials again at a point it
//!   draws within its turn, so its jitter is one turn at most. Its turn is kept under its peer: back at or after its
//!   turn's start, it's admitted whatever the count, so no runner is refused twice in a herd. Placed a turn apart, each
//!   one's wait for the control loop stays under a turn (Lindley's recursion: drawn within their turns, their jitters
//!   telescope to less than one), so the count passes the limit by two at most (that wait, and the jitter), and by
//!   what letting go of the runners refused meanwhile costs the loop, in turns, while the line passes them.
//! - **With nothing learned** of `work`, no turn can be given: a runner over the limit waits, what it sends let go,
//!   until the first admission settles; then each runner waiting is given its turn in the order it came.
//! - **Restarts.** As it stops, the server keeps when it began to (kiln.db's kv, [`STOPPED`]); as it next answers
//!   runners it learns how long that took. With none learned, it expects as long as this start took, from its process's
//!   start to answering runners: a start is most of a restart, and its stop and its supervisor's (or Kubernetes')
//!   restart aren't known until a restart is seen.
//! - **The drain's notice**: on its way down, the server tells each runner that takes it when it expects to answer
//!   again, and that runner's turn after it, a turn apart in the order of their peers ([`Admission::back`]).
//! - **What things cost** is learned on `kiln_http::turn::Costs`, the program's one sketch of what work costs: buckets
//!   by powers of two that forget by halving every window, the window what a mean of the samples' own spread needs to
//!   reach the runtime's precision (`hyper_rt::machine::wake::sample_window`). The values above are its means, and the
//!   limit is its rule (`Costs::fit`): the most admissions whose control-loop work, reckoned at the dearest it holds,
//!   runs past an admission alone by no more than a turn, what putting one off costs it.
//!
//! Written by the hub's control loop alone (an admission's events, its runner's link coming and going, the control
//! loop's own turns), read anywhere: counts in atomics, each runner's stage in a map swapped whole (`crate::rcu`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hyper_transport::PeerId;
use kiln_http::turn::Costs;

/// Where what's learned of admissions is kept, in kiln.db's `kv`: an admission alone, then the control loop's work on
/// one, each a mean in nanoseconds (`-`: nothing learned).
pub const KEY: &str = "runners/admission";
/// Where what's learned of this server's restarts is kept: their mean, in milliseconds (`-`: nothing learned).
pub const RESTARTS: &str = "server/restarts";
/// When this server last began to stop, in milliseconds since the Unix epoch: kept as it does, taken as it next answers.
pub const STOPPED: &str = "server/stopped";

/// What letting a runner go has cost the control loop so far, and so far on its connection that's open.
type Spent = Arc<AtomicU64>;

/// When a runner over the limit is to dial again: `after` from now, at a point it draws within `width`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Turn {
    pub after: Duration,
    pub width: Duration,
}

impl Turn {
    /// As the wire says it, in microseconds (`ServerMsg::Later`, `ServerMsg::Back`).
    pub fn micros(&self) -> (u64, u64) {
        let us = |d: Duration| u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
        (us(self.after), us(self.width))
    }
}

/// What a runner's `Admit` gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Taken: its Hello and catch-up follow.
    Admitted,
    /// Over the limit: told its turn, what it sends let go until it closes.
    Later(Turn),
    /// Over the limit with nothing learned of a turn: told its turn once the first admission settles.
    Waiting,
}

/// Where a runner's admission stands.
#[derive(Clone, Debug)]
enum Stage {
    /// Admitted at `since`, the `begun`-th admission (from 0); alone so far if none was in progress as it began; the
    /// control loop's time on its events so far (`own`), and on other runners' as it began (`other`).
    Admitted { since: Instant, begun: u64, alone: bool, own: Arc<AtomicU64>, other: u64 },
    /// Over the limit with nothing learned of a turn, the `seq`-th to wait: what it sends is let go.
    Waiting { seq: u64, spent: Spent },
    /// Over the limit, its turn beginning at `turn`, kept for it: what it sends is let go until it comes back.
    Placed { turn: Instant, spent: Spent },
    /// Told of the drain: its link closes.
    Leaving,
}

/// The server's admission of runners reconnecting, and what it knows of its own restarts.
pub struct Admission {
    /// What `line` is measured from.
    epoch: Instant,
    in_progress: AtomicU32,
    /// Admissions begun, ever.
    begun: AtomicU64,
    /// Runners that waited, ever: their order.
    waited: AtomicU64,
    /// The end of the last turn given, nanoseconds after `epoch`.
    line: AtomicU64,
    /// The control loop's time on other runners' messages, ever, while any runner was admitted or let go.
    other: AtomicU64,
    /// The control loop's work on one admission; an admission alone, Admit to Settled; letting a runner's connection
    /// go; and this server's restarts, in milliseconds (its restarts are seconds to days: in nanoseconds they'd all
    /// share the sketch's top bucket, which stops at 2^31).
    work: Costs,
    alone: Costs,
    let_go: Costs,
    restarts: Costs,
    /// This start, from its process's start to answering runners (nanoseconds; 0 until it answers).
    start: AtomicU64,
    stages: crate::rcu::Rcu<crate::util::FastMap<PeerId, Stage>>,
    /// Runners told of the drain still connected, and the last of them closing.
    leaving: AtomicU32,
    left: crate::rt::Changed,
}

/// `at` as nanoseconds after `epoch`.
fn after_epoch(epoch: Instant, at: Instant) -> u64 {
    u64::try_from(at.saturating_duration_since(epoch).as_nanos()).unwrap_or(u64::MAX)
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// A sketch's mean as kept: whole units, or `-`.
fn kept_mean(c: &Costs) -> String {
    c.mean().map_or_else(|| "-".to_owned(), |m| (m as u64).to_string())
}

/// `N` means as kept ([`kept_mean`]), each none for `-`: none if the text doesn't read as just that.
fn kept_means<const N: usize>(text: Option<&str>) -> Option<[Option<u64>; N]> {
    let mut words = text?.split_whitespace();
    let mut out = [None; N];
    for slot in &mut out {
        let w = words.next()?;
        *slot = if w == "-" { None } else { Some(w.parse().ok()?) };
    }
    words.next().is_none().then_some(out)
}

impl Admission {
    /// Nothing learned yet; `now` as the server's clock has it.
    pub fn new(now: Instant) -> Self {
        Self {
            epoch: now,
            in_progress: AtomicU32::new(0),
            begun: AtomicU64::new(0),
            waited: AtomicU64::new(0),
            line: AtomicU64::new(0),
            other: AtomicU64::new(0),
            work: Costs::nothing_yet(),
            alone: Costs::nothing_yet(),
            let_go: Costs::nothing_yet(),
            restarts: Costs::nothing_yet(),
            start: AtomicU64::new(0),
            stages: crate::rcu::Rcu::new(crate::util::FastMap::default()),
            leaving: AtomicU32::new(0),
            left: crate::rt::Changed::new(),
        }
    }

    /// Admissions in progress.
    pub fn in_progress(&self) -> u32 {
        self.in_progress.load(Ordering::Relaxed)
    }

    /// A turn: the control loop's work on one admission, once learned.
    pub fn turn(&self) -> Option<Duration> {
        self.work.mean().map(|ns| Duration::from_nanos(ns as u64)).filter(|d| !d.is_zero())
    }

    /// An admission alone, once learned.
    fn alone(&self) -> Option<Duration> {
        self.alone.mean().map(|ns| Duration::from_nanos(ns as u64))
    }

    /// Admissions in progress at once: the rule work is weighed by (`Costs::fit`), the most admissions whose control-loop
    /// work, reckoned at the dearest the sketch holds, runs past an admission alone by no more than a turn, what putting
    /// one off costs it. Little's law with the spread in: at the mean, `alone / work` rounded down, and one more. One
    /// until both are learned.
    pub fn limit(&self) -> u32 {
        match (self.alone(), self.turn()) {
            (Some(alone), Some(turn)) => self.work.fit(i64::try_from(nanos(alone)).unwrap_or(i64::MAX), nanos(turn)).map_or(1, |n| u32::try_from(n).unwrap_or(u32::MAX).max(1)),
            _ => 1,
        }
    }

    fn stage(&self, peer: PeerId) -> Option<Stage> {
        self.stages.load().get(&peer).cloned()
    }

    /// Whether what `peer`'s connection sends is let go: it's waiting for its turn, or placed and not back yet.
    pub fn letting_go(&self, peer: PeerId) -> bool {
        matches!(self.stages.load().get(&peer), Some(Stage::Waiting { .. } | Stage::Placed { .. }))
    }

    /// What letting a runner's connection go costs the control loop, as learned: none before any was.
    fn letting_go_costs(&self) -> Duration {
        self.let_go.mean().map_or(Duration::ZERO, |ns| Duration::from_nanos(ns as u64))
    }

    /// When the last turn given ends.
    fn line_end(&self) -> Instant {
        kiln_seams::clock::later(self.epoch, Duration::from_nanos(self.line.load(Ordering::Relaxed)))
    }

    /// The next turn's start for one with `in_progress` admissions ahead of it, the turns already given after them
    /// and the control loop's time to come on letting it go (`shift`): taken, its end the line's.
    fn place(&self, now: Instant, in_progress: u32, turn: Duration, shift: Duration) -> Instant {
        let ahead = kiln_seams::clock::later(now, turn.saturating_mul(in_progress));
        let start = ahead.max(kiln_seams::clock::later(self.line_end(), shift));
        self.line.store(after_epoch(self.epoch, kiln_seams::clock::later(start, turn)), Ordering::Relaxed);
        start
    }

    /// `peer` said `Admit` at `now`: admitted, told its turn, or left waiting for one. `enforce`: the limit is
    /// declared (`[server] admission`); undeclared, every runner is admitted and its admission measured all the same.
    pub fn admit(&self, peer: PeerId, now: Instant, enforce: bool) -> Verdict {
        let stage = self.stage(peer);
        match &stage {
            // Connected again while its admission is in progress (its last link not yet known lost): it keeps its place.
            Some(Stage::Admitted { .. }) => return Verdict::Admitted,
            // Back before its turn: its turn again, not a new one.
            Some(Stage::Placed { turn, .. }) if *turn > now => {
                return Verdict::Later(Turn { after: turn.saturating_duration_since(now), width: self.turn().unwrap_or_default() });
            }
            // Waiting, and nothing learned yet: waiting still, in its place.
            Some(Stage::Waiting { .. }) if self.turn().is_none() && self.in_progress() > 0 => return Verdict::Waiting,
            _ => {}
        }
        let in_progress = self.in_progress();
        let placed = matches!(stage, Some(Stage::Placed { .. }));
        if placed || !enforce || (in_progress < self.limit() && self.line_end() <= now) {
            let begun = self.begun.fetch_add(1, Ordering::Relaxed);
            self.in_progress.fetch_add(1, Ordering::Relaxed);
            let admitted = Stage::Admitted { since: now, begun, alone: in_progress == 0, own: Arc::new(AtomicU64::new(0)), other: self.other.load(Ordering::Relaxed) };
            self.stages.update(|m| m.insert(peer, admitted.clone()));
            return Verdict::Admitted;
        }
        let spent: Spent = Arc::new(AtomicU64::new(0));
        match self.turn() {
            Some(turn) => {
                let start = self.place(now, in_progress, turn, self.letting_go_costs());
                self.stages.update(|m| m.insert(peer, Stage::Placed { turn: start, spent: Arc::clone(&spent) }));
                Verdict::Later(Turn { after: start.saturating_duration_since(now), width: turn })
            }
            None => {
                let seq = self.waited.fetch_add(1, Ordering::Relaxed);
                self.stages.update(|m| m.insert(peer, Stage::Waiting { seq, spent: Arc::clone(&spent) }));
                Verdict::Waiting
            }
        }
    }

    /// The control loop held an event of `peer`'s for `took`: its admission's work, what letting it go costs, or, while
    /// any runner is admitted or being let go, the loop's other work.
    pub fn worked(&self, peer: PeerId, took: Duration) {
        let stages = self.stages.load();
        if stages.is_empty() {
            return;
        }
        let ns = nanos(took);
        match stages.get(&peer) {
            Some(Stage::Admitted { own, .. }) => own.fetch_add(ns, Ordering::Relaxed),
            Some(Stage::Waiting { spent, .. } | Stage::Placed { spent, .. }) => spent.fetch_add(ns, Ordering::Relaxed),
            Some(Stage::Leaving) | None => self.other.fetch_add(ns, Ordering::Relaxed),
        };
    }

    /// `peer` said `Settled` at `now`: its admission ends, and what it took is learned. The first learned of a turn
    /// gives every runner waiting for one its turn, each to be told.
    pub fn settled(&self, peer: PeerId, now: Instant) -> Vec<(PeerId, Turn)> {
        let Some(Stage::Admitted { since, begun, alone, own, other }) = self.stage(peer) else { return Vec::new() };
        self.stages.update(|m| m.remove(&peer));
        self.in_progress.fetch_sub(1, Ordering::Relaxed);
        let first = self.turn().is_none();
        // Its own time on the loop, over the share of the loop the other runners left it while it was in progress.
        let took = nanos(now.saturating_duration_since(since));
        let spare = took.saturating_sub(self.other.load(Ordering::Relaxed).saturating_sub(other));
        let own = own.load(Ordering::Relaxed);
        self.work.learn(1, if spare == 0 { own } else { (own as f64 * took as f64 / spare as f64) as u64 });
        if alone && self.begun.load(Ordering::Relaxed) == begun.saturating_add(1) {
            self.alone.learn(1, took);
        }
        if first { self.give_turns(now) } else { Vec::new() }
    }

    /// Every runner waiting for a turn given one, in the order they came.
    fn give_turns(&self, now: Instant) -> Vec<(PeerId, Turn)> {
        let Some(turn) = self.turn() else { return Vec::new() };
        let mut waiting: Vec<(u64, PeerId, Spent)> = self.stages.load().iter().filter_map(|(p, s)| if let Stage::Waiting { seq, spent } = s { Some((*seq, *p, Arc::clone(spent))) } else { None }).collect();
        if waiting.is_empty() {
            return Vec::new();
        }
        waiting.sort_unstable_by_key(|(seq, _, _)| *seq);
        // What letting them go cost the loop is behind it: their frames came as they waited.
        let placed: Vec<(PeerId, Instant, Spent)> = waiting.into_iter().map(|(_, p, spent)| (p, self.place(now, self.in_progress(), turn, Duration::ZERO), spent)).collect();
        self.stages.update(|m| {
            for (p, start, spent) in &placed {
                m.insert(*p, Stage::Placed { turn: *start, spent: Arc::clone(spent) });
            }
        });
        placed.into_iter().map(|(p, start, _)| (p, Turn { after: start.saturating_duration_since(now), width: turn })).collect()
    }

    /// `peer`'s link closed at `now`. An admission in progress is given up, teaching nothing; a runner let go teaches
    /// what letting its connection go cost the loop, and a runner waiting forgets it waited (it says `Admit` again as it
    /// comes back) while a turn is kept. With nothing learned yet and none in progress any more, the first runner waiting
    /// is told to come now: the next admission is what teaches a turn.
    pub fn closed(&self, peer: PeerId, now: Instant) -> Vec<(PeerId, Turn)> {
        match self.stage(peer) {
            Some(Stage::Admitted { .. }) => {
                self.stages.update(|m| m.remove(&peer));
                if self.in_progress.fetch_sub(1, Ordering::Relaxed) == 1 && self.turn().is_none() {
                    return self.come_now(now);
                }
                Vec::new()
            }
            Some(Stage::Waiting { spent, .. }) => {
                self.stages.update(|m| m.remove(&peer));
                self.let_go.learn(1, spent.swap(0, Ordering::Relaxed));
                Vec::new()
            }
            Some(Stage::Placed { spent, .. }) => {
                self.let_go.learn(1, spent.swap(0, Ordering::Relaxed));
                Vec::new()
            }
            Some(Stage::Leaving) => {
                self.stages.update(|m| m.remove(&peer));
                if self.leaving.fetch_sub(1, Ordering::Relaxed) == 1 {
                    self.left.notify();
                }
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    /// The first runner waiting, placed now.
    fn come_now(&self, now: Instant) -> Vec<(PeerId, Turn)> {
        let first = self.stages.load().iter().filter_map(|(p, s)| if let Stage::Waiting { seq, spent } = s { Some((*seq, *p, Arc::clone(spent))) } else { None }).min_by_key(|(seq, _, _)| *seq);
        let Some((_, peer, spent)) = first else { return Vec::new() };
        self.stages.update(|m| m.insert(peer, Stage::Placed { turn: now, spent: Arc::clone(&spent) }));
        vec![(peer, Turn { after: Duration::ZERO, width: Duration::ZERO })]
    }

    /// The server answers runners again, its process `started` before; `stopped`: how long ago it began to stop last,
    /// if it kept that. Learns the restart, giving what to keep ([`RESTARTS`]).
    pub fn answering(&self, started: Duration, stopped: Option<Duration>) -> Option<String> {
        self.start.store(nanos(started).max(1), Ordering::Relaxed);
        let took = stopped?;
        self.restarts.learn(1, u64::try_from(took.as_millis()).unwrap_or(u64::MAX));
        Some(kept_mean(&self.restarts))
    }

    /// How long a restart of this server is expected to take: what it learned of its restarts, else as long as this
    /// start took; none before it answers.
    pub fn restart(&self) -> Option<Duration> {
        self.restarts.mean().map(|ms| Duration::from_millis(ms as u64)).or_else(|| Some(Duration::from_nanos(self.start.load(Ordering::Relaxed))).filter(|d| !d.is_zero()))
    }

    /// The `k`-th runner's notice (from 0, in the order of their peers) as the server goes down `restart` from its
    /// answering again: after it, `k` turns, a turn wide.
    pub fn back(&self, k: usize, restart: Duration) -> Turn {
        let turn = self.turn().unwrap_or_default();
        Turn { after: restart.saturating_add(turn.saturating_mul(u32::try_from(k).unwrap_or(u32::MAX))), width: turn }
    }

    /// `peers` told of the drain: a wait for the last of them to close its link.
    pub fn leaving(&self, peers: &[PeerId]) -> crate::rt::Listen<'_> {
        let told = u32::try_from(peers.len()).unwrap_or(u32::MAX);
        self.stages.update(|m| {
            for p in peers {
                m.insert(*p, Stage::Leaving);
            }
        });
        self.leaving.store(told, Ordering::Relaxed);
        let listen = self.left.listen();
        if told == 0 {
            self.left.notify();
        }
        listen
    }

    /// What's learned of admissions, to keep ([`KEY`]).
    pub fn kept(&self) -> String {
        format!("{} {}", kept_mean(&self.alone), kept_mean(&self.work))
    }

    /// Takes what a run before kept ([`KEY`], [`RESTARTS`]), each mean as one sample, which what this run learns soon
    /// outweighs; a text that doesn't read leaves nothing learned.
    pub fn take_kept(&self, admissions: Option<&str>, restarts: Option<&str>) {
        if let Some([alone, work]) = kept_means::<2>(admissions) {
            for (c, mean) in [(&self.alone, alone), (&self.work, work)] {
                if let Some(ns) = mean {
                    c.learn(1, ns);
                }
            }
        }
        if let Some([Some(ms)]) = kept_means::<1>(restarts) {
            self.restarts.learn(1, ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BinaryHeap, VecDeque};

    use super::*;
    use crate::agent::redial;

    const US: Duration = Duration::from_micros(1);
    const MS: Duration = Duration::from_millis(1);
    const S: Duration = Duration::from_secs(1);

    /// An admission whose learned values are `alone` and `work`, each from a sample that says just that.
    fn learned(alone: Duration, work: Duration) -> Admission {
        let a = Admission::new(Instant::now());
        a.alone.learn(1, nanos(alone));
        a.work.learn(1, nanos(work));
        a
    }

    /// R5: Little's law over the control loop, by the rule work is weighed by. Admissions alone take 7 ms of which the
    /// control loop's work is 2 ms: four at once (3.5 rounded down, and one more), a turn is 2 ms, and runners over the
    /// limit are placed after every admission ahead of them, a turn each, then a turn apart, each told a turn's width to
    /// draw within.
    #[test]
    fn r5_the_limit_is_littles_law_over_the_control_loop() {
        let a = learned(7 * MS, 2 * MS);
        assert_eq!((a.limit(), a.turn()), (4, Some(2 * MS)));
        let t0 = a.epoch;
        for p in 1..=4 {
            assert_eq!(a.admit(p, t0, true), Verdict::Admitted);
        }
        assert_eq!(a.in_progress(), 4);
        // Four ahead: the first placed a turn after each, the next a turn later.
        assert_eq!(a.admit(5, t0, true), Verdict::Later(Turn { after: 8 * MS, width: 2 * MS }));
        assert_eq!(a.admit(6, t0 + MS, true), Verdict::Later(Turn { after: 9 * MS, width: 2 * MS }));
        assert!(a.letting_go(5) && a.letting_go(6) && !a.letting_go(1), "what the placed send is let go");
        // Back early: told its own turn again, the line not moved; back at it: admitted past the limit.
        assert_eq!(a.admit(5, t0 + 3 * MS, true), Verdict::Later(Turn { after: 5 * MS, width: 2 * MS }));
        assert_eq!(a.admit(7, t0 + 3 * MS, true), Verdict::Later(Turn { after: 9 * MS, width: 2 * MS }), "the line ran to 12 ms");
        assert_eq!(a.admit(5, t0 + 8 * MS, true), Verdict::Admitted);
        assert_eq!(a.in_progress(), 5, "at its turn, past the limit");
        // Undeclared, nothing is refused, and every admission is measured all the same.
        let open = learned(7 * MS, 2 * MS);
        for p in 1..=9 {
            assert_eq!(open.admit(p, t0, false), Verdict::Admitted);
        }
        assert_eq!(open.in_progress(), 9);
    }

    /// R5: what an admission takes is learned from what the server measures. The control loop's time on a runner's own
    /// events, Admit to Settled, is its admission's work, over the share of the loop the other runners' messages left it
    /// meanwhile; an admission is alone when none was in progress as it began and none began before it settled; one
    /// given up teaches nothing. What letting a runner go costs the loop is learned apart, and pushes the line back by
    /// that as each runner is refused.
    #[test]
    fn r5_work_and_alone_are_learned_from_the_control_loops_time() {
        let a = Admission::new(Instant::now());
        let t0 = a.epoch;
        a.worked(1, 5 * MS);
        assert_eq!(a.admit(1, t0, true), Verdict::Admitted);
        a.worked(1, MS);
        a.worked(1, 2 * MS);
        assert!(a.settled(1, t0 + 6 * MS).is_empty());
        assert_eq!((a.turn(), a.alone(), a.limit()), (Some(3 * MS), Some(6 * MS), 3));
        // Two overlapping: neither alone, each its own work.
        assert_eq!(a.admit(1, t0 + 10 * MS, true), Verdict::Admitted);
        assert_eq!(a.admit(2, t0 + 11 * MS, true), Verdict::Admitted);
        a.worked(1, 3 * MS);
        a.worked(2, 3 * MS);
        a.settled(1, t0 + 14 * MS);
        a.settled(2, t0 + 16 * MS);
        assert_eq!((a.alone(), a.turn()), (Some(6 * MS), Some(3 * MS)), "neither was alone");
        assert_eq!(a.admit(3, t0 + 20 * MS, true), Verdict::Admitted);
        a.worked(3, 9 * MS);
        assert!(a.closed(3, t0 + 21 * MS).is_empty());
        assert_eq!((a.in_progress(), a.turn()), (0, Some(3 * MS)), "given up: nothing learned");
        // Other runners' messages took half the loop while one was in progress: its 2 ms of work is a turn of 4 ms.
        let b = Admission::new(Instant::now());
        let t0 = b.epoch;
        assert_eq!(b.admit(1, t0, true), Verdict::Admitted);
        b.worked(1, 2 * MS);
        b.worked(9, 3 * MS);
        b.settled(1, t0 + 6 * MS);
        assert_eq!((b.turn(), b.limit()), (Some(4 * MS), 2));
        let t1 = t0 + 10 * MS;
        assert_eq!(b.admit(2, t1, true), Verdict::Admitted);
        assert_eq!(b.admit(3, t1, true), Verdict::Admitted);
        assert_eq!(b.admit(4, t1, true), Verdict::Later(Turn { after: 8 * MS, width: 4 * MS }));
        b.worked(4, MS);
        b.worked(2, MS);
        assert!(b.closed(4, t1).is_empty(), "its turn kept");
        assert_eq!(b.admit(5, t1, true), Verdict::Later(Turn { after: 13 * MS, width: 4 * MS }), "a turn later, and later still by the 1 ms letting the last go cost");
    }

    /// R5: with nothing learned, one at a time (the next when the last is done), and a runner over that waits, what
    /// it sends let go, until the first admission settles: then each runner waiting is told its turn, in the order it
    /// came, a turn apart from now. Should the one in progress go first, the first waiting is told to come now.
    #[test]
    fn r5_with_nothing_learned_runners_wait_for_the_first_admission() {
        let a = Admission::new(Instant::now());
        let t0 = a.epoch;
        assert_eq!((a.limit(), a.turn()), (1, None));
        assert_eq!(a.admit(1, t0, true), Verdict::Admitted);
        for p in [7, 3, 5] {
            assert_eq!(a.admit(p, t0, true), Verdict::Waiting);
            assert!(a.letting_go(p));
        }
        assert_eq!(a.admit(3, t0, true), Verdict::Waiting, "waiting, it says Admit again: still waiting, in its place");
        a.worked(7, 9 * MS);
        a.worked(1, 4 * MS);
        let told = a.settled(1, t0 + 5 * MS);
        let w = 4 * MS;
        assert_eq!(told, vec![(7, Turn { after: Duration::ZERO, width: w }), (3, Turn { after: w, width: w }), (5, Turn { after: 2 * w, width: w })], "in the order they came");
        assert_eq!(a.limit(), 2, "5 ms alone over 4 ms of work");
        // Given up before anything was learned: the first waiting comes now.
        let b = Admission::new(Instant::now());
        let t0 = b.epoch;
        assert_eq!(b.admit(1, t0, true), Verdict::Admitted);
        assert_eq!(b.admit(2, t0, true), Verdict::Waiting);
        assert_eq!(b.admit(4, t0, true), Verdict::Waiting);
        assert!(b.closed(4, t0).is_empty(), "a runner waiting that closes forgets it waited");
        assert_eq!(b.closed(1, t0 + MS), vec![(2, Turn { after: Duration::ZERO, width: Duration::ZERO })]);
        assert_eq!(b.admit(2, t0 + 2 * MS, true), Verdict::Admitted, "at its turn");
    }

    /// The drain's notice: each runner told the expected restart and its turn after it, a turn apart; the restart is
    /// the mean of those learned from the stops kept, and with none learned it's as long as this start took.
    #[test]
    fn r5_the_expected_restart_is_learned_and_this_start_before_any() {
        let a = learned(7 * MS, 2 * MS);
        assert_eq!(a.restart(), None, "before it answers");
        assert_eq!(a.answering(3 * S, None), None);
        assert_eq!(a.restart(), Some(3 * S), "nothing learned: as long as this start");
        assert_eq!(a.answering(3 * S, Some(8 * S)).as_deref(), Some("8000"));
        assert_eq!(a.restart(), Some(8 * S));
        a.answering(3 * S, Some(6 * S));
        assert_eq!(a.restart(), Some(7 * S));
        let r = 10 * S;
        assert_eq!(a.back(0, r), Turn { after: r, width: 2 * MS });
        assert_eq!(a.back(3, r), Turn { after: r + 6 * MS, width: 2 * MS });
        assert_eq!(Admission::new(Instant::now()).back(3, r), Turn { after: r, width: Duration::ZERO }, "no turn learned: all at the restart");
    }

    /// What's learned is kept as text a restart reads back, each mean as one sample; a text that doesn't read leaves
    /// nothing learned.
    #[test]
    fn r5_what_is_learned_is_kept() {
        let a = learned(7 * MS, 2 * MS);
        assert!(a.answering(S, Some(5 * S)).is_some());
        assert_eq!(a.kept(), "7000000 2000000");
        let b = Admission::new(Instant::now());
        b.take_kept(Some(&a.kept()), Some(&kept_mean(&a.restarts)));
        assert_eq!((b.limit(), b.turn(), b.restart()), (4, Some(2 * MS), Some(5 * S)));
        let c = Admission::new(Instant::now());
        c.take_kept(Some("- 2000000"), Some("-"));
        assert_eq!((c.limit(), c.turn(), c.restart()), (1, Some(2 * MS), None), "a mean not learned is none");
        for bad in ["", "1", "x 2", "1 2 3"] {
            let d = Admission::new(Instant::now());
            d.take_kept(Some(bad), None);
            assert_eq!((d.limit(), d.turn()), (1, None), "{bad:?}");
        }
        for bad in ["", "x", "1 2"] {
            let d = Admission::new(Instant::now());
            d.take_kept(None, Some(bad));
            assert_eq!(d.restart(), None, "{bad:?}");
        }
    }

    /// The program's bar for allocations: the control loop's turns while admissions are in progress, the limit and a
    /// turn allocate nothing; an admission's stage change copies the stage map once (its table, grown if the copy was
    /// full, and the `Arc` it's swapped in by), and an admission keeps a count of the loop's time on it (an `Arc`).
    #[test]
    fn r5_admissions_allocate_only_their_stage_maps_copies() {
        let a = learned(7 * MS, 2 * MS);
        let t0 = a.epoch;
        assert_eq!(a.admit(1, t0, true), Verdict::Admitted);
        let before = crate::os::counted::made();
        for _ in 0..100 {
            a.worked(1, US);
            a.worked(2, US);
        }
        let _ = (a.limit(), a.turn(), a.letting_go(1), a.in_progress());
        assert_eq!(crate::os::counted::made().blocks, before.blocks, "a turn held, the limit, a turn");
        let before = crate::os::counted::made();
        assert_eq!(a.admit(2, t0, true), Verdict::Admitted);
        let one = crate::os::counted::made().blocks - before.blocks;
        assert!(one <= 4, "{one} allocations admitting");
        let before = crate::os::counted::made();
        for p in 3..=9 {
            assert!(matches!(a.admit(p, t0, true), Verdict::Admitted | Verdict::Later(_)));
        }
        assert!(crate::os::counted::made().blocks - before.blocks <= 4 * 7, "a stage map's copy each, and an admission's count");
    }

    // ------------------------------------------------------------------------------------------------- the herd

    /// What a simulated reconnect costs the server's control loop, and the link.
    #[derive(Clone, Copy)]
    struct Costs {
        /// A runner's Admit, and each frame of a runner being let go.
        admit: Duration,
        drop: Duration,
        /// Frames its connection sends after its Admit, before its Welcome: Machine, Instance, Hello, Build, Caps …
        burst: u32,
        /// What they cost the control loop when it's admitted (its Hello's work, mostly).
        hello: Duration,
        /// What it sends on its Welcome (its catch-up), on the control loop, before its Settled.
        catch_up: Duration,
        /// A round trip on the link (a dial's handshake is one).
        rtt: Duration,
    }

    const COSTS: Costs = Costs { admit: Duration::from_micros(20), drop: Duration::from_micros(5), burst: 8, hello: Duration::from_millis(4), catch_up: Duration::from_micros(600), rtt: Duration::from_millis(1) };

    /// What comes to the control loop, in the order it comes.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Item {
        Admit(PeerId),
        /// The frames after a runner's Admit, as one: taken if it's admitted, let go if not.
        Burst(PeerId),
        CatchUp(PeerId),
        Settled(PeerId),
        /// Another runner's message: the control loop's other work.
        Other,
    }

    /// What happens next, by when.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum Ev {
        /// The runner's redials say dial now.
        Dial(PeerId),
        /// A dial answered: its Admit and its burst come in.
        Connected(PeerId),
        /// An item reaches the control loop's queue.
        Queue(usize),
        /// The control loop is free to take the next.
        Loop,
        /// The server's `Later` reaches the runner: dial again after so many nanoseconds, within so many.
        Later(PeerId, u64, u64),
        /// Its `Welcome` reaches the runner: its catch-up and its Settled come in.
        Welcome(PeerId),
    }

    struct Runner {
        redials: redial::Redials,
        refused: u32,
        /// When its first Admit came to the server, and when it was welcomed.
        came: Option<Instant>,
        welcomed: Option<Instant>,
    }

    /// What one herd did.
    struct Herd {
        /// From the server answering to the last admission settled.
        took: Duration,
        /// When the first runner's first Admit came, and the last runner's.
        first: Duration,
        last: Duration,
        refused_most: u32,
        welcomed: usize,
        /// The longest another runner's message waited for the control loop.
        other_waited: Duration,
        /// The most admissions in progress at once past the limit then.
        over: u32,
        limit: u32,
        turn: Duration,
        alone: Duration,
    }

    /// `n` runners that lost a server gone for `down`, each redialing as its own redials say (it learned the server's
    /// restarts take `down`), the server answering again at `admission`'s epoch with `costs` for each reconnect; its
    /// limit declared or not (`enforce`). Another runner's message comes to the control loop every `other_every`.
    fn herd(n: u32, admission: &Admission, enforce: bool, costs: Costs, down: Duration, other_every: Duration) -> Herd {
        let t0 = admission.epoch;
        let lost = t0.checked_sub(down).unwrap_or(t0);
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut draw = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let learned = redial::Learned::of(&[costs.rtt; 3], &[down; 20]);
        let mut at: BinaryHeap<std::cmp::Reverse<(Instant, u64, Ev)>> = BinaryHeap::new();
        let mut order = 0u64;
        let mut push = |at: &mut BinaryHeap<std::cmp::Reverse<(Instant, u64, Ev)>>, when: Instant, ev: Ev| {
            order += 1;
            at.push(std::cmp::Reverse((when, order, ev)));
        };
        let mut runners: BTreeMap<PeerId, Runner> = BTreeMap::new();
        for p in 2..2 + PeerId::from(n) {
            let mut r = Runner { redials: redial::Redials::new(learned, None, lost), refused: 0, came: None, welcomed: None };
            r.redials.dialing(lost);
            assert!(r.redials.connected(lost).is_ok());
            r.redials.failed(lost, draw());
            if let Some(d) = r.redials.deadline() {
                push(&mut at, d, Ev::Dial(p));
            }
            runners.insert(p, r);
        }
        let mut items: Vec<(Instant, Item)> = Vec::new();
        let mut queue: VecDeque<(Instant, Item)> = VecDeque::new();
        let (mut looping, mut other_waited, mut over, mut last_settled, mut others) = (false, Duration::ZERO, 0u32, t0, t0);
        while let Some(std::cmp::Reverse((now, _, ev))) = at.pop() {
            while others <= now && runners.values().any(|r| r.welcomed.is_none()) {
                items.push((others, Item::Other));
                push(&mut at, others, Ev::Queue(items.len() - 1));
                others += other_every;
            }
            match ev {
                Ev::Dial(p) => {
                    let Some(r) = runners.get_mut(&p) else { continue };
                    if r.welcomed.is_some() || r.redials.on_deadline(now) != redial::Step::Dial {
                        continue;
                    }
                    r.redials.dialing(now);
                    if now < t0 {
                        // Nothing there yet: the dial fails at once, the next is drawn.
                        r.redials.failed(now, draw());
                        if let Some(d) = r.redials.deadline() {
                            push(&mut at, d, Ev::Dial(p));
                        }
                    } else {
                        push(&mut at, now + costs.rtt, Ev::Connected(p));
                    }
                }
                Ev::Connected(p) => {
                    let Some(r) = runners.get_mut(&p) else { continue };
                    assert!(r.redials.connected(now).is_ok());
                    let came = now + costs.rtt / 2;
                    if r.came.is_none() {
                        r.came = Some(came);
                    }
                    for item in [Item::Admit(p), Item::Burst(p)] {
                        items.push((came, item));
                        push(&mut at, came, Ev::Queue(items.len() - 1));
                    }
                }
                Ev::Queue(k) => {
                    if let Some(&item) = items.get(k) {
                        queue.push_back(item);
                    }
                    if !looping {
                        looping = true;
                        push(&mut at, now, Ev::Loop);
                    }
                }
                Ev::Loop => {
                    let Some((came, item)) = queue.pop_front() else {
                        looping = false;
                        continue;
                    };
                    let (peer, took) = match item {
                        Item::Admit(p) => {
                            if let Verdict::Later(t) = admission.admit(p, now, enforce) {
                                push(&mut at, now + costs.rtt / 2, Ev::Later(p, t.after.as_nanos() as u64, t.width.as_nanos() as u64));
                            }
                            over = over.max(admission.in_progress().saturating_sub(admission.limit()));
                            (p, costs.admit)
                        }
                        Item::Burst(p) if admission.letting_go(p) => (p, costs.drop * costs.burst),
                        Item::Burst(p) => {
                            push(&mut at, now + costs.hello + costs.rtt / 2, Ev::Welcome(p));
                            (p, costs.hello)
                        }
                        Item::CatchUp(p) => (p, costs.catch_up),
                        Item::Settled(p) => {
                            for (q, t) in admission.settled(p, now) {
                                push(&mut at, now + costs.rtt / 2, Ev::Later(q, t.after.as_nanos() as u64, t.width.as_nanos() as u64));
                            }
                            last_settled = now;
                            (p, costs.admit)
                        }
                        Item::Other => {
                            other_waited = other_waited.max(now.saturating_duration_since(came));
                            (1, costs.admit)
                        }
                    };
                    admission.worked(peer, took);
                    push(&mut at, now + took, Ev::Loop);
                }
                Ev::Later(p, after, width) => {
                    let Some(r) = runners.get_mut(&p) else { continue };
                    // As `crate::agent` takes `ServerMsg::Later`: its next dial at its turn from now, drawn within a
                    // turn, and its link closed.
                    r.refused += 1;
                    r.redials.later(now, Duration::from_nanos(after), Duration::from_nanos(width), draw());
                    r.redials.failed(now, draw());
                    if let Some(d) = r.redials.deadline() {
                        push(&mut at, d, Ev::Dial(p));
                    }
                }
                Ev::Welcome(p) => {
                    if let Some(r) = runners.get_mut(&p) {
                        r.welcomed = Some(now);
                    }
                    for item in [Item::CatchUp(p), Item::Settled(p)] {
                        items.push((now + costs.rtt / 2, item));
                        push(&mut at, now + costs.rtt / 2, Ev::Queue(items.len() - 1));
                    }
                }
            }
        }
        let since = |at: Option<Instant>| at.map_or(Duration::MAX, |at| at.saturating_duration_since(t0));
        Herd {
            took: last_settled.saturating_duration_since(t0),
            first: runners.values().map(|r| since(r.came)).min().unwrap_or_default(),
            last: runners.values().map(|r| since(r.came)).max().unwrap_or_default(),
            refused_most: runners.values().map(|r| r.refused).max().unwrap_or(0),
            welcomed: runners.values().filter(|r| r.welcomed.is_some()).count(),
            other_waited,
            over,
            limit: admission.limit(),
            turn: admission.turn().unwrap_or_default(),
            alone: admission.alone().unwrap_or_default(),
        }
    }

    /// R5's done-when (docs/resilience.md): 50 and 500 runners reconnecting after a restart, each redialing as its own
    /// redials say, are all admitted, none refused more than once; the herd takes what the server's measured rate
    /// allows, a turn an admission after the first came (or the arrivals' own spread, if longer), plus the first
    /// admission and the last's, the last's jitter and wait (a turn each) and what letting the refused runners go costs
    /// the loop, not a picked spread; admissions in progress pass the limit by two at most, and by that letting go in
    /// turns; and the control loop's other work waits behind a limit's worth of admissions and that letting go, not the
    /// herd. Both with nothing learned (the first herd after this lands) and with what that herd taught (every one
    /// after). With the limit undeclared, as the old code took runners, every runner comes in as it dials and the same
    /// other work waits for much of the herd.
    #[test]
    fn r5_herds_of_50_and_500_are_admitted_at_the_measured_rate_each_refused_at_most_once() {
        let down = 5 * S;
        for n in [50u32, 500] {
            let first = Admission::new(Instant::now());
            let taught = herd(n, &first, true, COSTS, down, 5 * MS);
            let kept = Admission::new(Instant::now());
            kept.take_kept(Some(&first.kept()), None);
            let old = herd(n, &Admission::new(Instant::now()), false, COSTS, down, 5 * MS);
            assert_eq!(old.welcomed, usize::try_from(n).unwrap_or(0));
            // What the control loop spends letting a runner's first frames go: its Admit and its burst, unread.
            let letting_go = (COSTS.admit + COSTS.drop * COSTS.burst) * n;
            for (what, h) in [("nothing learned", taught), ("learned", herd(n, &kept, true, COSTS, down, 5 * MS))] {
                assert_eq!(h.welcomed, usize::try_from(n).unwrap_or(0), "{n}, {what}: every runner admitted");
                assert!(h.refused_most <= 1, "{n}, {what}: a runner refused {} times", h.refused_most);
                let bound = (h.first + h.turn * n).max(h.last) + 2 * h.alone + 2 * h.turn + letting_go;
                assert!(h.took <= bound, "{n}, {what}: took {:?}, the measured rate allows {bound:?} (turn {:?}, alone {:?}, first {:?}, last {:?})", h.took, h.turn, h.alone, h.first, h.last);
                let over = 2 + u32::try_from(letting_go.as_nanos().div_ceil(h.turn.as_nanos().max(1))).unwrap_or(u32::MAX);
                assert!(h.over <= over, "{n}, {what}: {} in progress past the limit {} (at most {over})", h.over, h.limit);
                let bar = h.alone * (h.limit + 1) + letting_go;
                assert!(h.other_waited <= bar, "{n}, {what}: other work waited {:?}, past a limit's worth of admissions ({bar:?})", h.other_waited);
                assert!(old.other_waited > bar, "{n}, undeclared: other work waited {:?}, within the declared limit's bar {bar:?}", old.other_waited);
            }
        }
    }
}
