//! The seeded scheduler for code that runs as tasks: each node of a simulation has its own
//! executor (kiln-seams' `SimSpawn`), all share one simulated clock (kiln-seams' `SimClock`), and
//! the seed picks which node's ready task is polled next, one poll at a time. A node that crashes
//! has every task dropped wherever it waits: a crash can come at any await.
//!
//! A model drives it: [`World::step`] makes one poll (or says none was ready), and when nothing is
//! ready the model either acts (its own events, the nemesis) or moves time to what's due next
//! ([`World::next_deadline`]).

use std::sync::Arc;
use std::time::Duration;

use kiln_seams::Spawn;
use kiln_seams::clock::SimClock;
use kiln_seams::spawn::{Ran, SimSpawn};

use crate::net::Node;
use crate::rng::Rng;

/// Most tasks one node runs at once.
pub const NODE_TASKS: usize = 10_000;

struct Exec {
    spawn: SimSpawn,
    /// How many times it has crashed.
    lives: u64,
}

fn executor() -> SimSpawn {
    // One poll a run: the world, not the executor, decides what's polled next.
    SimSpawn::with_limits(NODE_TASKS, 1)
}

/// Nodes running tasks on one simulated clock.
pub struct World {
    pub clock: SimClock,
    rng: Rng,
    nodes: Vec<Exec>,
    polls: u64,
}

impl World {
    pub fn new(clock: SimClock, rng: Rng) -> Self {
        Self { clock, rng, nodes: Vec::new(), polls: 0 }
    }

    /// A new node, with an executor of its own.
    pub fn add_node(&mut self) -> Node {
        let n = Node(u32::try_from(self.nodes.len()).unwrap_or(u32::MAX));
        self.nodes.push(Exec { spawn: executor(), lives: 0 });
        n
    }

    /// What runs `n`'s tasks, in its life now (a crash ends it: tasks spawned on it after are
    /// refused, `Stopped`).
    pub fn spawner(&self, n: Node) -> Option<Arc<dyn Spawn>> {
        let e = self.nodes.get(usize::try_from(n.0).ok()?)?;
        Some(Arc::new(e.spawn.clone()))
    }

    /// Tasks `n` runs now.
    pub fn tasks(&self, n: Node) -> usize {
        usize::try_from(n.0).ok().and_then(|i| self.nodes.get(i)).map_or(0, |e| e.spawn.tasks())
    }

    /// `n` crashes: every task it ran is dropped wherever it waits, and it starts its next life
    /// with an executor of its own (what the model starts on it again).
    pub fn crash(&mut self, n: Node) {
        if let Some(e) = usize::try_from(n.0).ok().and_then(|i| self.nodes.get_mut(i)) {
            e.spawn.stop();
            e.spawn = executor();
            e.lives = e.lives.saturating_add(1);
        }
    }

    /// How many times `n` has crashed.
    pub fn lives(&self, n: Node) -> u64 {
        usize::try_from(n.0).ok().and_then(|i| self.nodes.get(i)).map_or(0, |e| e.lives)
    }

    /// One poll of one ready task, of a node the seed picks among those with one: which node, or
    /// none when no task anywhere was ready.
    pub fn step(&mut self) -> Option<Node> {
        let n = self.nodes.len();
        if n == 0 {
            return None;
        }
        let start = self.rng.index(n);
        for k in 0..n {
            let i = start.saturating_add(k).checked_rem(n).unwrap_or(0);
            let Some(e) = self.nodes.get(i) else { continue };
            if matches!(e.spawn.run_until_stalled(), Ran::Budget | Ran::Stalled { polls: 1.. }) {
                self.polls = self.polls.saturating_add(1);
                return Some(Node(u32::try_from(i).unwrap_or(u32::MAX)));
            }
        }
        None
    }

    /// Polls until no task anywhere is ready, at most `budget` polls: how many it made, or none
    /// when tasks were still ready at the budget's end (one waking itself forever).
    pub fn settle(&mut self, budget: u64) -> Option<u64> {
        (0..budget).find(|_| self.step().is_none())
    }

    /// Polls made, ever.
    pub fn polls(&self) -> u64 {
        self.polls
    }

    /// Time since the simulation started.
    pub fn now(&self) -> Duration {
        self.clock.elapsed()
    }

    /// When the next sleep of any task is due, as time since the start.
    pub fn next_deadline(&self) -> Option<Duration> {
        let start = kiln_seams::Clock::now(&self.clock).checked_sub(self.clock.elapsed())?;
        self.clock.next_deadline().map(|t| t.saturating_duration_since(start))
    }

    /// Moves time to `at` (since the start), waking the sleeps due by then: nothing when `at`
    /// isn't ahead.
    pub fn advance_to(&self, at: Duration) {
        let now = self.clock.elapsed();
        if at > now {
            self.clock.advance(at.saturating_sub(now));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::FutureExt as _;
    use kiln_seams::Clock as _;
    use kiln_seams::spawn::spawn;
    use parking_lot::Mutex;

    use super::*;

    /// A task that notes each of its steps, yielding between them.
    async fn steps(name: &'static str, n: u32, log: Arc<Mutex<Vec<String>>>) {
        for i in 0..n {
            log.lock().push(format!("{name}{i}"));
            let mut yielded = false;
            futures::future::poll_fn(|cx| {
                if yielded {
                    return std::task::Poll::Ready(());
                }
                yielded = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            })
            .await;
        }
    }

    fn run(seed: u64) -> Vec<String> {
        let mut w = World::new(SimClock::new(), Rng::new(seed));
        let (a, b) = (w.add_node(), w.add_node());
        let log = Arc::new(Mutex::new(Vec::new()));
        for (node, name) in [(a, "a"), (b, "b")] {
            let sp = w.spawner(node);
            assert!(sp.is_some_and(|sp| spawn(sp.as_ref(), steps(name, 5, Arc::clone(&log))).is_ok()));
        }
        assert_eq!(w.settle(1000), Some(12));
        log.lock().clone()
    }

    /// CP-T6.1, AC-T6.1.4: the seed picks the interleaving, one poll at a time, and a seed's is the
    /// same every time.
    #[test]
    fn the_seed_picks_the_interleaving_and_a_seed_replays_it() {
        assert_eq!(run(1), run(1));
        let orders: std::collections::BTreeSet<Vec<String>> = (0..20).map(run).collect();
        assert!(orders.len() > 1, "different seeds interleave differently");
    }

    /// CP-T6.1, the goal's nemesis (a crash at every await): a crash drops every task of its node
    /// where it waits; what comes after the wait never runs; the node starts its next life with an
    /// executor of its own.
    #[test]
    fn a_crash_drops_a_node_s_tasks_where_they_wait() {
        let clock = SimClock::new();
        let mut w = World::new(clock.clone(), Rng::new(5));
        let n = w.add_node();
        let after = Arc::new(Mutex::new(false));
        let (c, a) = (clock.clone(), Arc::clone(&after));
        let sp = w.spawner(n);
        assert!(sp.is_some_and(|sp| spawn(sp.as_ref(), async move {
            c.sleep(Duration::from_secs(5)).await;
            *a.lock() = true;
        })
        .is_ok()));
        assert_eq!(w.settle(10), Some(1));
        assert_eq!(w.next_deadline(), Some(Duration::from_secs(5)));
        w.crash(n);
        w.advance_to(Duration::from_secs(10));
        assert_eq!(w.settle(10), Some(0));
        assert!(!*after.lock() && w.tasks(n) == 0 && w.lives(n) == 1 && clock.sleepers() == 0);
        let sp = w.spawner(n);
        assert!(sp.is_some_and(|sp| spawn(sp.as_ref(), async { 1 }).map(|h| h.now_or_never()).is_ok()));
    }
}
