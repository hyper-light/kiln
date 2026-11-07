//! How long a runner is given to come back (docs/machines.md, "The grace to come back"): once the server has lost it, or
//! has started again without it, its jobs wait that long for it before they're requeued, an attach to a session it
//! holds waits that long for it as the server starts (`crate::harness::returning`), and a job pod's record is kept that
//! long.
//!
//! The grace is the wait that costs least (research 01a1b3bbac36, f2 and f11). Waiting `t` for a lost runner costs the
//! time until it's back if it's back by `t`, else `t` and a requeue; the least expected cost is where the comebacks'
//! hazard falls to one over a requeue's cost (Pandey et al., "Optimizing Waiting Thresholds Within A State Machine").
//! Both are measured: how long runners took to come back holding work, counted by bit length in nanoseconds (a DDSketch
//! at the coarsest log scale, as `crate::agent::redial`'s: no precision to pick), and what a requeue costs, the time
//! until a requeued job's step starts on another runner. The rule can stop only at a bucket's top, so the grace is one,
//! never short of the comebacks it waits for. Until both have been seen, the grace is [`prior`]. What's learned is kept
//! in kiln.db (`kv`, [`KEY`]) as it changes.
//!
//! The server's own stalls aren't charged to runners (Lifeguard's local health): a comeback counts until its hello came
//! off the wire, not until the server took it in, and after a restart runners are charged from when the server began
//! hearing them ([`Comeback::hearing`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// Where what's learned is kept, in kiln.db's `kv`: `<requeues' ns>/<requeues>`, then `<bucket>:<comebacks>` for each
/// bucket that has any.
pub const KEY: &str = "runners/comeback";

/// Comebacks by bit length in nanoseconds: bucket `i` holds `[2^(i-1), 2^i)`, bucket 0 none at all.
const BUCKETS: usize = u64::BITS as usize + 1;

/// The grace before any runner has come back holding work or any requeue's cost is known: the longest a runner that
/// has learned nothing of its dials takes to dial again once it has lost the server, by its own redials (a dial given
/// up, then the wait before the next).
pub fn prior() -> Duration {
    crate::agent::redial::unlearned_bound()
}

/// How long runners took to come back, what requeues cost, and the grace they give.
pub struct Comeback {
    learned: Mutex<Learned>,
    /// The grace in nanoseconds, as `learned` last gave it.
    grace_ns: AtomicU64,
    /// When the server began hearing its runners.
    hearing: OnceLock<Instant>,
}

struct Learned {
    /// How many runners came back holding work within each bucket's time.
    comebacks: [u64; BUCKETS],
    /// What the requeues measured cost, in nanoseconds, and how many there were.
    requeue_ns: u64,
    requeues: u64,
    /// Jobs requeued that haven't started a step since, each until it does or ends, or its run does.
    requeued: HashMap<String, Requeued>,
}

struct Requeued {
    run: String,
    /// The runner it left.
    left: String,
    at: Instant,
}

impl Default for Comeback {
    fn default() -> Self {
        Self {
            learned: Mutex::new(Learned { comebacks: [0; BUCKETS], requeue_ns: 0, requeues: 0, requeued: HashMap::new() }),
            grace_ns: AtomicU64::new(nanos(prior())),
            hearing: OnceLock::new(),
        }
    }
}

impl Comeback {
    /// How long a runner is given to come back, now.
    pub fn grace(&self) -> Duration {
        Duration::from_nanos(self.grace_ns.load(Ordering::Relaxed))
    }

    /// A runner came back holding its work `after` it was lost (or the server started without it). Gives what to keep.
    pub fn came_back(&self, after: Duration) -> String {
        let mut learned = self.lock();
        if let Some(n) = learned.comebacks.get_mut(bucket(after)) {
            *n = n.saturating_add(1);
        }
        self.decide(&learned);
        learned.kept()
    }

    /// Job `job` of run `run`, which ran on runner `on`, was requeued at `at`.
    pub fn requeued(&self, job: &str, run: &str, on: &str, at: Instant) {
        self.lock().requeued.insert(job.to_owned(), Requeued { run: run.to_owned(), left: on.to_owned(), at });
    }

    /// A step of job `job` started on runner `on` at `at`. When it's the first since the job was requeued off another
    /// runner, that requeue's cost is learned: gives what to keep.
    pub fn step_started(&self, job: &str, on: &str, at: Instant) -> Option<String> {
        let mut learned = self.lock();
        let requeued = learned.requeued.remove(job)?;
        if requeued.left == on {
            return None;
        }
        learned.requeue_ns = learned.requeue_ns.saturating_add(nanos(at.saturating_duration_since(requeued.at)));
        learned.requeues = learned.requeues.saturating_add(1);
        self.decide(&learned);
        Some(learned.kept())
    }

    /// Job `job` ended: no requeue of it waits for a step.
    pub fn ended(&self, job: &str) {
        self.lock().requeued.remove(job);
    }

    /// Run `run` ended, or was deleted: no requeue of its jobs waits for a step.
    pub fn run_ended(&self, run: &str) {
        self.lock().requeued.retain(|_, r| r.run != run);
    }

    /// The runs of the jobs whose requeues wait for a step.
    pub fn requeued_runs(&self) -> Vec<String> {
        let mut runs: Vec<String> = self.lock().requeued.values().map(|r| r.run.clone()).collect();
        runs.sort_unstable();
        runs.dedup();
        runs
    }

    /// Whether a requeue of job `job` waits for a step.
    #[cfg(test)]
    pub(crate) fn waits(&self, job: &str) -> bool {
        self.lock().requeued.contains_key(job)
    }

    /// The server began hearing its runners at `at`.
    pub fn hearing(&self, at: Instant) {
        self.hearing.get_or_init(|| at);
    }

    /// How long the server has been hearing its runners: what one not back since it started is charged.
    pub fn heard_for(&self) -> Duration {
        self.hearing.get().map_or(Duration::ZERO, Instant::elapsed)
    }

    /// Takes what a run before kept.
    pub fn take_kept(&self, kept: &str) -> anyhow::Result<()> {
        let mut words = kept.split_whitespace();
        let (ns, n) = words.next().and_then(|w| w.split_once('/')).ok_or_else(|| anyhow::anyhow!("{kept:?} doesn't start with what requeues cost"))?;
        let (requeue_ns, requeues): (u64, u64) = (ns.parse()?, n.parse()?);
        let mut comebacks = [0; BUCKETS];
        for w in words {
            let (i, n) = w.split_once(':').ok_or_else(|| anyhow::anyhow!("{w:?} isn't a bucket's count"))?;
            *comebacks.get_mut(i.parse::<usize>()?).ok_or_else(|| anyhow::anyhow!("there's no bucket {i}"))? = n.parse()?;
        }
        let mut learned = self.lock();
        (learned.comebacks, learned.requeue_ns, learned.requeues) = (comebacks, requeue_ns, requeues);
        self.decide(&learned);
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Learned> {
        self.learned.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn decide(&self, learned: &Learned) {
        let grace = learned.least_cost().unwrap_or_else(|| nanos(prior()));
        self.grace_ns.store(grace, Ordering::Relaxed);
    }
}

impl Learned {
    /// The wait in nanoseconds whose expected cost is least: none before a comeback and a requeue's cost are known.
    /// Only a bucket's top can be least: between two, each moment waited costs every runner still out.
    fn least_cost(&self) -> Option<u64> {
        let all = self.comebacks.iter().fold(0u64, |a, n| a.saturating_add(*n));
        if all == 0 || self.requeues == 0 {
            return None;
        }
        let requeue = self.requeue_ns as f64 / self.requeues as f64;
        let (mut back, mut waited, mut least) = (0u64, 0.0, (0.0, all as f64 * requeue));
        for (i, n) in self.comebacks.iter().enumerate().filter(|(_, n)| **n > 0) {
            let t = top(i);
            back = back.saturating_add(*n);
            // DDSketch's estimate of a time in the bucket, within its relative error (a third) of any.
            waited += *n as f64 * t * 2.0 / 3.0;
            let cost = waited + all.saturating_sub(back) as f64 * (t + requeue);
            if cost < least.1 {
                least = (t, cost);
            }
        }
        Some(least.0 as u64)
    }

    fn kept(&self) -> String {
        let buckets = self.comebacks.iter().enumerate().filter(|(_, n)| **n > 0).map(|(i, n)| format!(" {i}:{n}"));
        std::iter::once(format!("{}/{}", self.requeue_ns, self.requeues)).chain(buckets).collect()
    }
}

fn bucket(d: Duration) -> usize {
    (u64::BITS - nanos(d).leading_zeros()) as usize
}

/// Bucket `i`'s top, in nanoseconds.
fn top(i: usize) -> f64 {
    (i as f64).exp2()
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    /// The top of the bucket `d` falls in.
    fn top_of(d: Duration) -> Duration {
        Duration::from_nanos(top(bucket(d)) as u64)
    }

    /// A grace learned from `comebacks` and one requeue that cost `requeue`.
    fn learned(comebacks: &[Duration], requeue: Duration) -> Comeback {
        let c = Comeback::default();
        for after in comebacks {
            c.came_back(*after);
        }
        let at = Instant::now();
        c.requeued("job", "run", "lost", at);
        c.step_started("job", "other", at + requeue);
        c
    }

    /// Research 01a1b3bbac36 f11: the grace is the wait that costs least. Nine in ten runners back in 2 s and one in ten
    /// in 20 s: with a requeue costing 5 s it waits for the first (to their bucket's top), not 18 s more for one in ten;
    /// with one costing 60 s, for both. Later comebacks, a later grace. Until a comeback and a requeue's cost are both
    /// known, the prior.
    #[test]
    fn comeback_the_grace_is_the_wait_that_costs_least_over_comebacks_and_requeues() {
        let mixed = |fast: u32| [vec![fast * S; 90], vec![20 * S; 10]].concat();
        assert_eq!(learned(&mixed(2), 5 * S).grace(), top_of(2 * S));
        assert_eq!(learned(&mixed(2), 60 * S).grace(), top_of(20 * S));
        assert_eq!(learned(&mixed(4), 5 * S).grace(), top_of(4 * S));
        let c = Comeback::default();
        c.came_back(2 * S);
        assert_eq!(c.grace(), prior(), "no requeue's cost yet");
        assert_eq!(learned(&[], 5 * S).grace(), prior(), "no comeback yet");
    }

    /// A requeue costs the time until its job's step starts on another runner; one on the runner it left is that
    /// runner's own work going on (it took the job back). A requeue waits no longer than its job or its run lasts.
    #[test]
    fn comeback_a_requeue_costs_until_its_job_steps_on_another_runner() {
        let c = Comeback::default();
        let at = Instant::now();
        c.requeued("j1", "r1", "box", at);
        assert_eq!(c.step_started("j1", "box", at + S), None, "back on the runner it left");
        assert_eq!(c.step_started("j1", "other", at + 2 * S), None, "taken back, it waited for no step");
        c.requeued("j2", "r1", "box", at);
        assert_eq!(c.step_started("j2", "other", at + 7 * S).as_deref(), Some("7000000000/1"));
        assert_eq!(c.step_started("j2", "other", at + 9 * S), None, "its first step only");
        for (job, run) in [("j3", "r1"), ("j4", "r2"), ("j5", "r2"), ("j6", "r3")] {
            c.requeued(job, run, "box", at);
        }
        assert_eq!(c.requeued_runs(), ["r1", "r2", "r3"]);
        c.ended("j3");
        c.run_ended("r2");
        assert!(!c.waits("j3") && !c.waits("j4") && !c.waits("j5") && c.waits("j6"));
        assert_eq!(c.requeued_runs(), ["r3"]);
    }

    /// A heavy tail doesn't blow the grace up: one runner back after 10 h among 99 back in 2 s leaves it at the 2 s
    /// ones' bucket. Comebacks with no mean (Lomax, shape 1/2, scale 1 s: hazard 1/(2 (1 s + t))) and a requeue costing
    /// 10 s give it at the top of the bucket where the hazard meets one over the requeue's cost, 4 s.
    #[test]
    fn comeback_a_heavy_tail_does_not_blow_the_grace_up() {
        let mut once = vec![2 * S; 99];
        once.push(36_000 * S);
        assert_eq!(learned(&once, 5 * S).grace(), top_of(2 * S));
        let n = 1000;
        let lomax: Vec<Duration> = (1..=n).map(|k| Duration::from_secs_f64((1.0 - f64::from(k) / f64::from(n + 1)).powf(-2.0) - 1.0)).collect();
        assert_eq!(learned(&lomax, 10 * S).grace(), top_of(4 * S));
    }

    /// No gain: what's learned is counts, so the order runners came back in changes nothing (an estimator with gains
    /// follows the latest).
    #[test]
    fn comeback_no_gain_the_order_runners_came_back_in_changes_nothing() {
        let times: Vec<Duration> = (1..=40).map(|k| k * k * S / 10).collect();
        let reversed: Vec<Duration> = times.iter().rev().copied().collect();
        let (a, b) = (learned(&times, 5 * S), learned(&reversed, 5 * S));
        assert_eq!(a.grace(), b.grace());
        assert_eq!(a.lock().kept(), b.lock().kept());
    }

    /// What's kept reads back as what was learned; a text that doesn't read is an error, and leaves the grace as it
    /// was.
    #[test]
    fn comeback_what_is_kept_reads_back_and_what_does_not_is_an_error() -> anyhow::Result<()> {
        let c = learned(&[2 * S, 3 * S, 40 * S], 5 * S);
        let kept = c.lock().kept();
        let again = Comeback::default();
        again.take_kept(&kept)?;
        assert_eq!((again.grace(), again.lock().kept()), (c.grace(), kept));
        for bad in ["", "4000 2000", "a/1", "5000/1 39", "5000/1 39:x", "5000/1 65:1", "5000/1 39:1 4000"] {
            let fresh = Comeback::default();
            anyhow::ensure!(fresh.take_kept(bad).is_err(), "{bad:?} read");
            assert_eq!(fresh.grace(), prior(), "{bad:?}");
        }
        Ok(())
    }
}
