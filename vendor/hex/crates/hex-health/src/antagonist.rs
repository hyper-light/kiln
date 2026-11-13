//! Antagonists, found by correlation and throttled with a cap (S8; CPI², Zhang et al., EuroSys '13,
//! PF[16]), as CPI² states it (§3, §4):
//!
//! - A job's CPI spec is the mean and standard deviation of its tasks' CPI samples. No CPI management
//!   for a job with fewer than [`MIN_JOB_TASKS`] tasks or fewer than [`MIN_TASK_SAMPLES`] samples per
//!   task, and samples from a task using under [`MIN_CPU_MILLI`] CPU-sec/sec are ignored.
//! - A sample is an outlier above the spec's 2σ point; a task is a victim when flagged
//!   [`VICTIM_FLAGS`] times in [`FLAG_WINDOW_NS`].
//! - Its antagonist is the co-tenant on its node whose CPU usage correlates best with the victim's CPI
//!   over [`CORRELATION_WINDOW_NS`], by CPI²'s own correlation, if at least [`MIN_CORRELATION_MILLI`]
//!   / 1000. At most one identification a second.
//! - The antagonist is capped at [`BEST_EFFORT_CAP_MILLI`] or [`CAP_MILLI`] CPU-sec/sec for
//!   [`THROTTLE_NS`]: throttled, not killed.
//!
//! The spec is learned from samples that aren't outliers, so an antagonist doesn't teach the spec its
//! own interference.

use std::collections::{BTreeMap, VecDeque};

use crate::HealthError;

const S: u64 = 1_000_000_000;

/// "fewer than 5 tasks or fewer than 100 CPI samples per task" (CPI² §3).
pub const MIN_JOB_TASKS: usize = 5;
pub const MIN_TASK_SAMPLES: u64 = 100;
/// "We ignore CPI measurements from tasks that use less than 0.25 CPU-sec/sec" (CPI² §3).
pub const MIN_CPU_MILLI: u64 = 250;
/// "larger than the 2σ point on the predicted CPI distribution" (CPI² §3).
pub const SIGMAS: u64 = 2;
/// "flagged as an outlier at least 3 times in a 5 minute window" (CPI² §3).
pub const VICTIM_FLAGS: usize = 3;
pub const FLAG_WINDOW_NS: u64 = 300 * S;
/// "we typically use a 10-minute window" (CPI² §4.1).
pub const CORRELATION_WINDOW_NS: u64 = 600 * S;
/// "requiring a correlation value of at least 0.35 works well" (CPI² §4.1).
pub const MIN_CORRELATION_MILLI: i64 = 350;
/// "at most one of these attempts is performed each second" (CPI² §4.2).
pub const IDENTIFY_GAP_NS: u64 = S;
/// "0.01 CPU-sec/sec for low-importance ("best effort") batch jobs and 0.1 CPU-sec/sec for other job
/// types", "applied for 5 minutes at a time" (CPI² §4.2).
pub const BEST_EFFORT_CAP_MILLI: u64 = 10;
pub const CAP_MILLI: u64 = 100;
pub const THROTTLE_NS: u64 = 300 * S;
/// Samples kept per task: ten minutes of CPI²'s one-a-minute sampling is 10; 64 holds a ten-second
/// cadence's minute and the window at one a minute with room. An older sample leaves the window first.
pub const HISTORY: usize = 64;

/// One CPI sample of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub task: u64,
    pub job: u64,
    pub node: u64,
    pub at_ns: u64,
    /// Cycles per instruction, in thousandths.
    pub cpi_milli: u64,
    /// CPU-sec/sec, in thousandths.
    pub cpu_milli: u64,
    pub best_effort: bool,
}

/// A cap on an antagonist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Throttle {
    pub antagonist: u64,
    pub victim: u64,
    pub cap_milli: u64,
    pub until_ns: u64,
    pub correlation_milli: i64,
}

#[derive(Debug, Clone, Default)]
struct Spec {
    count: u64,
    sum: u128,
    sum_sq: u128,
}

impl Spec {
    /// The 2σ point, in thousandths, once the job has a spec.
    fn threshold(&self) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let n = u128::from(self.count);
        let mean = self.sum.checked_div(n)?;
        let var = self
            .sum_sq
            .checked_div(n)?
            .saturating_sub(mean.saturating_mul(mean));
        let sigma = isqrt(var);
        let t = mean.saturating_add(sigma.saturating_mul(u128::from(SIGMAS)));
        u64::try_from(t).ok()
    }

    fn add(&mut self, cpi: u64) {
        let c = u128::from(cpi);
        self.count = self.count.saturating_add(1);
        self.sum = self.sum.saturating_add(c);
        self.sum_sq = self.sum_sq.saturating_add(c.saturating_mul(c));
    }
}

fn isqrt(v: u128) -> u128 {
    if v < 2 {
        return v;
    }
    // Newton's method from above; at most 128 rounds for a u128.
    let mut x = v;
    let mut y = x.div_ceil(2).max(1);
    let mut rounds: u32 = 0;
    while y < x && rounds < 128 {
        x = y;
        y = x.saturating_add(v.checked_div(x).unwrap_or(0)) >> 1;
        rounds = rounds.saturating_add(1);
    }
    x
}

#[derive(Debug, Clone)]
struct Task {
    job: u64,
    node: u64,
    best_effort: bool,
    samples: u64,
    history: VecDeque<(u64, u64, u64)>,
    flags: VecDeque<u64>,
}

#[derive(Debug, Clone)]
pub struct Antagonists {
    specs: BTreeMap<u64, Spec>,
    tasks: BTreeMap<u64, Task>,
    max_tasks: usize,
    last_identify_ns: Option<u64>,
    throttled: BTreeMap<u64, u64>,
}

impl Antagonists {
    pub fn new(max_tasks: usize) -> Self {
        Self {
            specs: BTreeMap::new(),
            tasks: BTreeMap::new(),
            max_tasks,
            last_identify_ns: None,
            throttled: BTreeMap::new(),
        }
    }

    pub fn remove_task(&mut self, task: u64) {
        self.tasks.remove(&task);
        self.throttled.remove(&task);
    }

    /// Whether `task` is under a cap at `now_ns`.
    pub fn throttled(&self, task: u64, now_ns: u64) -> bool {
        self.throttled.get(&task).is_some_and(|u| *u > now_ns)
    }

    /// Take one node's samples of one instant, as CPI²'s per-machine agent gathers every task's at
    /// once (§3); returns a new throttle when one made its task a victim whose antagonist was found.
    /// Every sample is recorded before a victim is judged, so its correlation sees its co-tenants'
    /// samples of the same instant whatever order they come in.
    pub fn sample(&mut self, samples: &[Sample]) -> Result<Option<Throttle>, HealthError> {
        let mut victim = None;
        for s in samples {
            let flagged = self.record(*s)?;
            if victim.is_none() {
                victim = flagged.map(|threshold| (s.task, s.at_ns, threshold));
            }
        }
        let Some((task, at_ns, threshold)) = victim else {
            return Ok(None);
        };
        if self
            .last_identify_ns
            .is_some_and(|last| at_ns < last.saturating_add(IDENTIFY_GAP_NS))
        {
            return Ok(None);
        }
        self.last_identify_ns = Some(at_ns);
        Ok(self.identify(task, at_ns, threshold))
    }

    /// Record one sample; returns the job's 2σ point when it made its task a victim.
    fn record(&mut self, s: Sample) -> Result<Option<u64>, HealthError> {
        if !self.tasks.contains_key(&s.task) && self.tasks.len() >= self.max_tasks {
            return Err(HealthError::Full {
                what: "sampled tasks",
                bound: self.max_tasks,
            });
        }
        let threshold = self.ready_threshold(s.job);
        let task = self.tasks.entry(s.task).or_insert_with(|| Task {
            job: s.job,
            node: s.node,
            best_effort: s.best_effort,
            samples: 0,
            history: VecDeque::with_capacity(HISTORY),
            flags: VecDeque::with_capacity(VICTIM_FLAGS),
        });
        if let Some(&(last, _, _)) = task.history.back()
            && s.at_ns < last
        {
            return Err(HealthError::TimeBackwards {
                what: "CPI samples",
                now_ns: s.at_ns,
                last_ns: last,
            });
        }
        task.node = s.node;
        if task.history.len() >= HISTORY {
            task.history.pop_front();
        }
        task.history.push_back((s.at_ns, s.cpi_milli, s.cpu_milli));
        let horizon = s.at_ns.saturating_sub(CORRELATION_WINDOW_NS);
        while task.history.front().is_some_and(|(t, _, _)| *t < horizon) {
            task.history.pop_front();
        }
        if s.cpu_milli < MIN_CPU_MILLI {
            return Ok(None);
        }
        task.samples = task.samples.saturating_add(1);
        let outlier = threshold.is_some_and(|t| s.cpi_milli > t);
        if !outlier {
            self.specs.entry(s.job).or_default().add(s.cpi_milli);
            return Ok(None);
        }
        let flag_horizon = s.at_ns.saturating_sub(FLAG_WINDOW_NS);
        while task.flags.front().is_some_and(|t| *t < flag_horizon) {
            task.flags.pop_front();
        }
        if task.flags.len() >= VICTIM_FLAGS {
            task.flags.pop_front();
        }
        task.flags.push_back(s.at_ns);
        if task.flags.len() < VICTIM_FLAGS {
            return Ok(None);
        }
        Ok(threshold)
    }

    /// The job's 2σ point once it has enough tasks with enough samples each.
    fn ready_threshold(&self, job: u64) -> Option<u64> {
        let ready = self
            .tasks
            .values()
            .filter(|t| t.job == job && t.samples >= MIN_TASK_SAMPLES)
            .count();
        if ready < MIN_JOB_TASKS {
            return None;
        }
        self.specs.get(&job).and_then(Spec::threshold)
    }

    fn identify(&mut self, victim: u64, now_ns: u64, threshold: u64) -> Option<Throttle> {
        let v = self.tasks.get(&victim)?;
        let mut best: Option<(i64, u64, bool)> = None;
        for (id, a) in &self.tasks {
            if *id == victim || a.node != v.node || self.throttled(*id, now_ns) {
                continue;
            }
            let Some(c) = correlation(&v.history, &a.history, threshold) else {
                continue;
            };
            if best.is_none_or(|(b, _, _)| c > b) {
                best = Some((c, *id, a.best_effort));
            }
        }
        let (corr, antagonist, best_effort) = best?;
        if corr < MIN_CORRELATION_MILLI {
            return None;
        }
        let until_ns = now_ns.saturating_add(THROTTLE_NS);
        self.throttled.insert(antagonist, until_ns);
        Some(Throttle {
            antagonist,
            victim,
            cap_milli: if best_effort {
                BEST_EFFORT_CAP_MILLI
            } else {
                CAP_MILLI
            },
            until_ns,
            correlation_milli: corr,
        })
    }
}

/// CPI² §4.1's correlation, in thousandths, over time-aligned samples: with the suspect's usage `u`
/// normalised to sum to 1, each pair adds `u·(1 − t/c)` when the victim's CPI `c` is above the
/// threshold `t` and `u·(c/t − 1)` when below. `None` with no aligned pair or no usage.
fn correlation(
    victim: &VecDeque<(u64, u64, u64)>,
    suspect: &VecDeque<(u64, u64, u64)>,
    threshold: u64,
) -> Option<i64> {
    if threshold == 0 {
        return None;
    }
    let t = i128::from(threshold);
    let mut total_u: i128 = 0;
    let mut sum: i128 = 0;
    for &(at, c, _) in victim {
        let Some(&(_, _, u)) = suspect.iter().find(|(st, _, _)| *st == at) else {
            continue;
        };
        let (u, c) = (i128::from(u), i128::from(c));
        if c == 0 {
            continue;
        }
        total_u = total_u.checked_add(u)?;
        // Scaled by 1e6 so the integer division keeps its precision.
        let term = if c > t {
            u.checked_mul(c.checked_sub(t)?)?
                .checked_mul(1_000_000)?
                .checked_div(c)?
        } else {
            u.checked_mul(c.checked_sub(t)?)?
                .checked_mul(1_000_000)?
                .checked_div(t)?
        };
        sum = sum.checked_add(term)?;
    }
    if total_u == 0 {
        return None;
    }
    let milli = sum.checked_div(total_u)?.checked_div(1000)?;
    i64::try_from(milli).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60 * S;

    /// A node of `tasks` tasks of job 1 (latency-sensitive) and one batch task 99 of job 2, sampled
    /// once a minute (CPI²'s cadence) for `warm` minutes; from `onset` the batch task burns CPU and
    /// the victims' CPI doubles. Returns the first throttle and when.
    fn run(onset: Option<u64>, minutes: u64) -> Option<(u64, Throttle)> {
        run_with(onset, minutes, (3000, 300), false)
    }

    /// `run`, the batch task using `cpu.0` while hot and `cpu.1` otherwise, its sample first in each
    /// instant's batch when `antagonist_first`.
    fn run_with(
        onset: Option<u64>,
        minutes: u64,
        cpu: (u64, u64),
        antagonist_first: bool,
    ) -> Option<(u64, Throttle)> {
        let mut a = Antagonists::new(64);
        for m in 0..minutes {
            let t = m * MIN;
            let hot = onset.is_some_and(|o| t >= o) && !a.throttled(99, t);
            let mut batch: Vec<Sample> = (0..6)
                .map(|task| {
                    let jitter = (m * 7 + task * 13) % 50;
                    Sample {
                        task,
                        job: 1,
                        node: 0,
                        at_ns: t,
                        cpi_milli: if hot { 2000 + jitter } else { 1000 + jitter },
                        cpu_milli: 800,
                        best_effort: false,
                    }
                })
                .collect();
            let antagonist = Sample {
                task: 99,
                job: 2,
                node: 0,
                at_ns: t,
                cpi_milli: 1500,
                cpu_milli: if hot { cpu.0 } else { cpu.1 },
                best_effort: true,
            };
            if antagonist_first {
                batch.insert(0, antagonist);
            } else {
                batch.push(antagonist);
            }
            if let Some(th) = a.sample(&batch).unwrap() {
                return Some((t, th));
            }
        }
        None
    }

    /// S8 antagonist: once job 1 has its spec, an antagonist's onset makes a victim within three
    /// samples, the batch task correlates above 0.35 and is capped at 0.01 CPU-sec/sec for 5 minutes.
    #[test]
    fn an_antagonist_is_found_by_correlation_and_capped() {
        let onset = 130 * MIN;
        let (at, th) = run(Some(onset), 200).unwrap();
        assert_eq!(th.antagonist, 99);
        assert_eq!(th.cap_milli, BEST_EFFORT_CAP_MILLI);
        assert_eq!(th.until_ns - at, THROTTLE_NS);
        assert!(
            th.correlation_milli >= MIN_CORRELATION_MILLI,
            "{}",
            th.correlation_milli
        );
        assert!(at - onset <= 3 * MIN, "{}", (at - onset) / MIN);
    }

    /// S8 antagonist (F4): the order of one instant's samples doesn't change the verdict. An
    /// antagonist at the class's least (its use tenfold, 4000 against 400; CPI doubled), sampled after
    /// its victims, is capped at the same sample as when sampled before them, within three samples.
    #[test]
    fn the_order_of_an_instants_samples_does_not_change_the_verdict() {
        let onset = 130 * MIN;
        let last = run_with(Some(onset), 200, (4000, 400), false).unwrap();
        let first = run_with(Some(onset), 200, (4000, 400), true).unwrap();
        assert_eq!(last, first);
        assert!(last.0 - onset <= 3 * MIN, "{}", (last.0 - onset) / MIN);
    }

    /// Non-vacuity: with no antagonist, nobody is throttled; before the spec is ready, nothing is.
    #[test]
    fn no_antagonist_no_throttle() {
        assert!(run(None, 200).is_none());
        assert!(run(Some(10 * MIN), 90).is_none());
    }

    /// CPI²'s correlation: usage that rises with the victim's CPI correlates positively; usage high
    /// only while the victim is well correlates negatively.
    #[test]
    fn correlation_has_cpi2s_sign() {
        let v: VecDeque<_> = [(1, 1000, 0), (2, 3000, 0)].into();
        let with: VecDeque<_> = [(1, 0, 10), (2, 0, 1000)].into();
        let against: VecDeque<_> = [(1, 0, 1000), (2, 0, 10)].into();
        assert!(correlation(&v, &with, 1500).unwrap() > 350);
        assert!(correlation(&v, &against, 1500).unwrap() < 0);
        assert_eq!(correlation(&v, &VecDeque::new(), 1500), None);
    }

    /// Hostile input: a full task map and time going backwards are typed refusals.
    #[test]
    fn hostile_input_is_refused_typed() {
        let mut a = Antagonists::new(1);
        let s = Sample {
            task: 1,
            job: 1,
            node: 0,
            at_ns: 10,
            cpi_milli: 0,
            cpu_milli: u64::MAX,
            best_effort: false,
        };
        a.sample(&[s]).unwrap();
        assert!(matches!(
            a.sample(&[Sample { at_ns: 5, ..s }]),
            Err(HealthError::TimeBackwards { .. })
        ));
        assert_eq!(
            a.sample(&[Sample { task: 2, ..s }]),
            Err(HealthError::Full {
                what: "sampled tasks",
                bound: 1
            })
        );
        assert_eq!(isqrt(u128::MAX), u128::from(u64::MAX));
    }
}
