//! Replays a recorded week of v1 runs under supersession policies (W25 step 1, plan
//! "Demand-driven CI: adopt, cover and stagger instead of cancel"; ci/sim/README.md).
//!
//! Every run arrives when it did, with its own jobs, `needs` and labels; each job takes its
//! recorded duration and outcome, or a draw where the trace doesn't say ([`draw`]); runners are
//! the trace's pools by label, as v1 places jobs (a free slot on a machine carrying every label, an
//! exclusive job only on an empty machine and nothing held back for it, a k8s job inside the
//! quota after its pod's start delay). What differs between policies is what a newer push does to
//! older work:
//!
//! - **P0**, the week as recorded: each run's concurrency group as its workflow had it, through
//!   [`crate::sched::groups`] (GitHub's semantics, which v1's are).
//! - **P1**, image.yaml's setting everywhere: the run in progress finishes, the newest pending
//!   run replaces older pending ones; bench grouped too.
//! - **P2**, adopt and withdraw: each job of a push is a demand keyed by its inputs at that commit.
//!   A newer push withdraws the older demands of its lane; it cancels nothing itself. A demand
//!   reuses a finished result with its key or adopts the execution in flight with it, else runs.
//!   Work no live demand wants is dropped if it hasn't started; once running it finishes, unless
//!   live work waits for its capacity and it has more than `finish_within` left; a cancelled k8s
//!   job's progress goes to the lane's next run of that job (`warm_credit`).
//! - **P3**, P2 plus verdicts: a commit without its own result is covered by a later one's
//!   ([`verdict`]); a new failure is bisected over the commits it may have come from.
//! - **P4**, P3 plus admission: bursts coalesce while their pools are saturated (`quiet`, never past
//!   `max_wait`), and exclusive or Mac jobs run on a cadence: the newest commit when the resource
//!   frees, an exclusive job's machine taking no new work while it waits.
//! - **P5**, P4 plus sharing: one UI build per UI tree and one release compile per source tree
//!   serve every pipeline that needs them.
//!
//! Pure, as docs/ci.md §0.3 asks: no clock, disk or randomness beyond the seed given; state is
//! kept in ordered maps, so the same trace, policy and seed give the same outcome, byte for byte.

pub mod draw;
pub mod land;
pub mod report;
pub mod rng;
pub mod synth;
pub mod trace;
pub mod verdict;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use crate::sched::groups::{GroupAction, GroupConfig, Groups};

use draw::Draw;
pub use trace::{End, Trace};

/// How a policy treats concurrency groups (P0, P1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupMode {
    /// As each run's workflow had it (P0).
    Recorded,
    /// `cancel-in-progress: false` for every workflow, each in a group (P1).
    Finish,
}

/// Coalescing (P4): a push waits `quiet` seconds while its pools are saturated, a burst never
/// longer than `max_wait`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coalesce {
    pub quiet: i64,
    pub max_wait: i64,
}

/// What a newer push does to older work.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub name: String,
    /// Run-level concurrency groups (P0, P1); `None`: demands (P2 and later).
    pub groups: Option<GroupMode>,
    /// P2: work no live demand wants is cancelled for live work only past this much left (s).
    pub finish_within: i64,
    /// P2: the share of a run a warm workspace can skip, from a cancelled k8s job's progress.
    pub warm_credit: f64,
    /// P3: covered and bisected verdicts.
    pub verdicts: bool,
    /// P4: coalescing.
    pub coalesce: Option<Coalesce>,
    /// P4: exclusive and Mac jobs on a cadence.
    pub cadence: bool,
    /// P5: shared steps; the share of the image's build its compile is (taken from release's).
    pub share: Option<f64>,
}

/// The defaults W25's later steps take, from the sweeps in ci/sim/README.md: finish work no one
/// wants when it has 5 min left (less waste than 2 min, within a point of its verdicts); a warm
/// workspace may skip half a run (no effect measured on the week); a quiet period of 1 min while
/// busy (the least waste, the most verdicts of those that coalesce); bursts held at most 10 min (no
/// burst of the week reached 5).
pub const FINISH_WITHIN: i64 = 300;
pub const WARM_CREDIT: f64 = 0.5;
pub const QUIET: i64 = 60;
pub const MAX_WAIT: i64 = 600;

impl Policy {
    fn demands(name: &str) -> Policy {
        Policy { name: name.into(), groups: None, finish_within: FINISH_WITHIN, warm_credit: WARM_CREDIT, verdicts: false, coalesce: None, cadence: false, share: None }
    }

    /// P0: today, as recorded (cancel-in-progress where the workflows said so).
    pub fn p0() -> Policy {
        Policy { groups: Some(GroupMode::Recorded), ..Policy::demands("P0") }
    }

    /// P1: finish the run in progress and replace the pending one (image.yaml's setting).
    pub fn p1() -> Policy {
        Policy { groups: Some(GroupMode::Finish), ..Policy::demands("P1") }
    }

    /// P2: adopt and withdraw.
    pub fn p2() -> Policy {
        Policy::demands("P2")
    }

    /// P3: P2 plus verdicts.
    pub fn p3() -> Policy {
        Policy { verdicts: true, ..Policy::demands("P3") }
    }

    /// P4: P3 plus coalescing and cadence.
    pub fn p4() -> Policy {
        Policy { verdicts: true, coalesce: Some(Coalesce { quiet: QUIET, max_wait: MAX_WAIT }), cadence: true, ..Policy::demands("P4") }
    }

    /// P5: P4 plus sharing across pipelines (the image's compile not shared: see ci/sim/README.md).
    pub fn p5() -> Policy {
        Policy { share: Some(0.0), ..Policy::p4() }.named("P5")
    }

    /// P0 to P5.
    pub fn all() -> Vec<Policy> {
        vec![Policy::p0(), Policy::p1(), Policy::p2(), Policy::p3(), Policy::p4(), Policy::p5()]
    }

    pub fn named(mut self, name: &str) -> Policy {
        self.name = name.into();
        self
    }
}

/// Why an execution stopped before its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stop {
    /// Its run was superseded by a newer one while it ran (P0).
    Superseded,
    /// No live demand wanted it, and live work needed its capacity (P2 and later).
    Unwanted,
    /// It never started: its run was cancelled, or its demands withdrawn, while it waited.
    Dropped,
}

/// How a job got its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum How {
    /// An execution of its own ran.
    Ran,
    /// It adopted an execution in flight with its key.
    Adopted,
    /// A finished result with its key was reused.
    Reused,
}

/// One execution, as it went.
#[derive(Debug, Clone, PartialEq)]
pub struct Exec {
    /// The demand that started it (its labels and draw).
    pub run: usize,
    pub job: usize,
    /// Its key: the job and its inputs; equal keys, equal work.
    pub key: String,
    pub queued: i64,
    pub started: Option<i64>,
    pub ended: Option<i64>,
    pub end: Option<End>,
    pub stop: Option<Stop>,
    pub pool: Option<usize>,
    /// What it took once started: its draw, less warm credit and shared steps.
    pub runtime: i64,
    /// Seconds of a cancelled predecessor's progress it skipped.
    pub credit: i64,
    /// Seconds of its own progress a successor skipped after it was cancelled.
    pub salvaged: i64,
    /// The bisection it ran for (P3), if any.
    pub bisect: Option<usize>,
}

impl Exec {
    /// Runner time it used, if it ended inside the window.
    pub fn used(&self) -> i64 {
        match (self.started, self.ended) {
            (Some(s), Some(e)) if e >= s => e - s,
            _ => 0,
        }
    }

    /// Runner time on work no one used: a superseded execution's, or an unwanted one's less what a
    /// successor reused.
    pub fn wasted(&self) -> i64 {
        match self.stop {
            Some(Stop::Superseded) => self.used(),
            Some(Stop::Unwanted) => (self.used() - self.salvaged).max(0),
            _ => 0,
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEnd {
    Passed,
    Failed,
    /// Cancelled (P0, P1), or withdrawn before all its jobs had results (P2 and later).
    Cancelled,
    /// Still going when the window ended.
    Open,
}

/// One run, as it went.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOut {
    pub end: RunEnd,
    /// When every job had its own result (passed or failed): its commit's direct verdict.
    pub concluded: Option<i64>,
}

/// One job of a run: when it got its result, how, and what it was.
#[derive(Debug, Clone, PartialEq)]
pub struct JobOut {
    /// When its result came (a skip, when its need failed).
    pub at: Option<i64>,
    pub end: Option<End>,
    pub how: Option<How>,
    pub skipped: bool,
    pub failed_step: Option<String>,
}

/// A bisection over the commits a new failure may have come from (P3).
#[derive(Debug, Clone, PartialEq)]
pub struct Bisection {
    pub lane: String,
    pub key: String,
    pub suspects: Vec<usize>,
    /// When its last run finished.
    pub done: Option<i64>,
}

/// Something the replay decided, and why (as docs/ci.md §0.3 asks of what kiln decides).
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub at: i64,
    pub run: usize,
    pub what: String,
}

/// What happened under one policy and seed.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub policy: String,
    pub seed: u64,
    /// Whether later results cover earlier commits (P3 and later).
    pub covers: bool,
    pub runs: Vec<RunOut>,
    pub jobs: Vec<Vec<JobOut>>,
    pub execs: Vec<Exec>,
    pub bisections: Vec<Bisection>,
    pub decisions: Vec<Decision>,
}

#[derive(Debug, Clone, PartialEq)]
enum JobSt {
    Waiting,
    Attached(usize),
    Done { at: i64, end: End, how: How },
    Skipped(i64),
    /// No result: its run was cancelled, or its demand withdrawn with no work for it.
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecSt {
    Queued,
    Launching,
    Running,
    Over,
}

/// At equal times, completions free capacity before arrivals take it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Ev {
    Finish(usize),
    Launched(usize),
    Release(usize),
    Arrive(usize),
}

impl Ev {
    fn order(self) -> u8 {
        match self {
            Ev::Finish(_) => 0,
            Ev::Launched(_) => 1,
            Ev::Release(_) => 2,
            Ev::Arrive(_) => 3,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct PoolSt {
    busy: u32,
    exclusive: bool,
    cpu: u64,
    mem: u64,
    pods: u32,
}

#[derive(Debug, Clone)]
struct RunSt {
    started: bool,
    /// The concurrency group it joined (P0, P1).
    joined: Option<String>,
    /// A newer push of its lane withdrew its demands (P2 and later).
    withdrawn: bool,
    /// Waiting out a coalescing quiet period (P4).
    held: bool,
    end: RunEnd,
    concluded: Option<i64>,
    jobs: Vec<JobSt>,
}

#[derive(Debug, Clone)]
struct BisectSt {
    run: usize,
    job: usize,
    left: u32,
}

/// A k8s job's requests when it gives none (the server's default_cpu and default_memory).
const DEFAULT_REQ: (u64, u64) = (1000, 2048);

/// Steps one pipeline's job produces for others (P5): (workflow, job) -> (step, input class).
const PRODUCES: &[(&str, &str, &str, &str)] = &[("ci", "ui", "ui", "ui"), ("release", "linux", "musl", "binary")];

/// The input class a shared step is keyed by.
fn step_class(step: &str) -> &'static str {
    match step {
        "ui" => "ui",
        _ => "binary",
    }
}

struct Engine<'a> {
    trace: &'a Trace,
    policy: &'a Policy,
    draws: Vec<Vec<Draw>>,
    /// Each job's typical duration: the median of its completed runs in the trace.
    typical: BTreeMap<(String, String), i64>,
    now: i64,
    seq: u64,
    events: BTreeMap<(i64, u8, u64), Ev>,
    queue: BTreeMap<(u8, i64, u64), usize>,
    pools: Vec<PoolSt>,
    groups: Groups,
    runs: Vec<RunSt>,
    execs: Vec<Exec>,
    exec_st: Vec<ExecSt>,
    /// The executions running now.
    running: BTreeSet<usize>,
    /// The demands each execution answers.
    serves: Vec<Vec<(usize, usize)>>,
    lanes: BTreeMap<String, Vec<usize>>,
    inflight: BTreeMap<String, usize>,
    ledger: BTreeMap<String, (End, Option<String>)>,
    warm: BTreeMap<(String, String), (i64, usize)>,
    held: BTreeMap<String, (usize, i64)>,
    steps_done: BTreeMap<(String, String), i64>,
    steps_inflight: BTreeMap<(String, String), (usize, i64)>,
    results: BTreeMap<(String, String), Vec<(usize, End, Option<String>)>>,
    bisections: Vec<Bisection>,
    bisect_st: Vec<BisectSt>,
    in_bisect: BTreeSet<(String, String, usize)>,
    decisions: Vec<Decision>,
}

/// Replays `trace` under `policy`, unknowns drawn from `seed`.
pub fn replay(trace: &Trace, policy: &Policy, seed: u64) -> Outcome {
    let mut took: BTreeMap<(String, String), Vec<i64>> = BTreeMap::new();
    for r in &trace.runs {
        for j in &r.jobs {
            if let Some((t, _)) = j.completed() {
                took.entry((r.workflow.clone(), j.key.clone())).or_default().push(t);
            }
        }
    }
    let typical = took
        .into_iter()
        .map(|(k, mut v)| {
            v.sort_unstable();
            let m = v.get(v.len() / 2).copied().unwrap_or(0);
            (k, m)
        })
        .collect();
    let mut e = Engine {
        trace,
        policy,
        draws: draw::draws(trace, seed),
        typical,
        now: 0,
        seq: 0,
        events: BTreeMap::new(),
        queue: BTreeMap::new(),
        pools: vec![PoolSt::default(); trace.pools.len()],
        groups: Groups::default(),
        runs: trace.runs.iter().map(|r| RunSt { started: false, joined: None, withdrawn: false, held: false, end: RunEnd::Open, concluded: None, jobs: vec![JobSt::Waiting; r.jobs.len()] }).collect(),
        execs: Vec::new(),
        exec_st: Vec::new(),
        running: BTreeSet::new(),
        serves: Vec::new(),
        lanes: BTreeMap::new(),
        inflight: BTreeMap::new(),
        ledger: BTreeMap::new(),
        warm: BTreeMap::new(),
        held: BTreeMap::new(),
        steps_done: BTreeMap::new(),
        steps_inflight: BTreeMap::new(),
        results: BTreeMap::new(),
        bisections: Vec::new(),
        bisect_st: Vec::new(),
        in_bisect: BTreeSet::new(),
        decisions: Vec::new(),
    };
    for (i, r) in trace.runs.iter().enumerate() {
        e.at(r.at, Ev::Arrive(i));
    }
    e.run();
    let mut jobs: Vec<Vec<JobOut>> = e.runs.iter().map(|rs| rs.jobs.iter().map(job_out).collect()).collect();
    // A failed job's failed step, from its draw: what verdicts compare a failure with.
    for (r, row) in jobs.iter_mut().enumerate() {
        for (j, out) in row.iter_mut().enumerate() {
            if matches!(out.end, Some(End::Failed | End::TimedOut)) {
                out.failed_step = e.draw(r, j).and_then(|d| d.failed_step.clone());
            }
        }
    }
    Outcome {
        policy: policy.name.clone(),
        seed,
        covers: policy.verdicts,
        runs: e.runs.iter().map(|r| RunOut { end: r.end, concluded: r.concluded }).collect(),
        jobs,
        execs: e.execs,
        bisections: e.bisections,
        decisions: e.decisions,
    }
}

fn job_out(st: &JobSt) -> JobOut {
    match st {
        JobSt::Done { at, end, how } => JobOut { at: Some(*at), end: Some(*end), how: Some(*how), skipped: false, failed_step: None },
        JobSt::Skipped(at) => JobOut { at: Some(*at), end: None, how: None, skipped: true, failed_step: None },
        _ => JobOut { at: None, end: None, how: None, skipped: false, failed_step: None },
    }
}

impl Engine<'_> {
    fn at(&mut self, t: i64, ev: Ev) {
        self.seq += 1;
        self.events.insert((t, ev.order(), self.seq), ev);
    }

    fn note(&mut self, run: usize, what: String) {
        self.decisions.push(Decision { at: self.now, run, what });
    }

    fn demand_mode(&self) -> bool {
        self.policy.groups.is_none()
    }

    fn run(&mut self) {
        while let Some(((t, _, _), ev)) = self.events.pop_first() {
            if t > self.trace.end {
                break;
            }
            self.now = t;
            match ev {
                Ev::Arrive(r) => self.arrive(r),
                Ev::Release(r) => self.release_held(r),
                Ev::Launched(x) => self.launched(x),
                Ev::Finish(x) => self.finish(x),
            }
            self.dispatch();
        }
    }

    fn lane_of(&self, r: usize) -> String {
        self.trace.runs.get(r).map_or_else(String::new, verdict::lane)
    }

    fn job_of(&self, r: usize, j: usize) -> Option<&trace::Job> {
        self.trace.runs.get(r).and_then(|run| run.jobs.get(j))
    }

    fn draw(&self, r: usize, j: usize) -> Option<&Draw> {
        self.draws.get(r).and_then(|row| row.get(j))
    }

    fn job_st(&self, r: usize, j: usize) -> Option<&JobSt> {
        self.runs.get(r).and_then(|rs| rs.jobs.get(j))
    }

    fn set_job(&mut self, r: usize, j: usize, st: JobSt) {
        if let Some(s) = self.runs.get_mut(r).and_then(|rs| rs.jobs.get_mut(j)) {
            *s = st;
        }
    }

    /// The key of a job's work at its run's commit: equal keys build the same inputs.
    fn key_of(&self, r: usize, j: usize) -> String {
        let trace = self.trace;
        let (Some(run), Some(job)) = (trace.runs.get(r), self.job_of(r, j)) else { return format!("run{r}/{j}") };
        let class = job.class.clone().unwrap_or_else(|| run.workflow.clone());
        match trace.commits.get(run.commit).and_then(|c| c.keys.get(&class)) {
            Some(k) => format!("{}/{}/{}", run.workflow, job.key, k),
            None => format!("{}/{}/run{r}", run.workflow, job.key),
        }
    }

    /// The group a run belongs to under the policy, and whether it cancels the run in progress.
    fn group_of(&self, r: usize) -> Option<(String, bool)> {
        let run = self.trace.runs.get(r)?;
        match self.policy.groups? {
            GroupMode::Recorded => run.group.clone().map(|g| (g, run.cancel_in_progress)),
            GroupMode::Finish => Some((run.group.clone().unwrap_or_else(|| format!("{}-{}-{}", run.workflow, run.event, run.branch)), false)),
        }
    }

    fn arrive(&mut self, r: usize) {
        let trace = self.trace;
        let Some(run) = trace.runs.get(r) else { return };
        if self.demand_mode() {
            let lane = self.lane_of(r);
            let older: Vec<usize> = self.lanes.get(&lane).cloned().unwrap_or_default();
            for o in older {
                self.withdraw(o, r);
            }
            self.lanes.entry(lane.clone()).or_default().push(r);
            if run.jobs.is_empty() {
                self.conclude(r, false);
                return;
            }
            self.start_run(r);
            if let Some(c) = self.policy.coalesce {
                let burst = self.held.remove(&lane).map(|(_, start)| start);
                if self.saturated(r) {
                    let start = burst.unwrap_or(self.now);
                    let until = (self.now + c.quiet).min(start + c.max_wait);
                    if until > self.now {
                        if let Some(rs) = self.runs.get_mut(r) {
                            rs.held = true;
                        }
                        self.held.insert(lane, (r, start));
                        self.note(r, format!("held until {until}: its pools are saturated (quiet {}, max_wait {})", c.quiet, c.max_wait));
                        self.at(until, Ev::Release(r));
                        return;
                    }
                }
            }
            self.ready(r);
            return;
        }
        if run.jobs.is_empty() {
            // A run whose plan failed: it ends at once, failed (as v1's do).
            self.conclude(r, false);
            return;
        }
        match self.group_of(r) {
            Some((g, cancel_in_progress)) => {
                if let Some(rs) = self.runs.get_mut(r) {
                    rs.joined = Some(g.clone());
                }
                let cfg = GroupConfig { cancel_in_progress, ..GroupConfig::default() };
                let actions = self.groups.arrive(&g, &g, &r.to_string(), &cfg);
                self.apply(&g, actions);
            }
            None => self.start_run(r),
        }
        // Jobs queue as their run arrives (v1 takes them in queue order once their run may start).
        self.ready(r);
    }

    fn release_held(&mut self, r: usize) {
        let Some(rs) = self.runs.get_mut(r) else { return };
        if !rs.held {
            return;
        }
        rs.held = false;
        let lane = self.lane_of(r);
        if self.held.get(&lane).is_some_and(|(h, _)| *h == r) {
            self.held.remove(&lane);
        }
        self.note(r, "released from its quiet period".into());
        self.ready(r);
    }

    /// A newer push `by` withdraws an older run's demands: what no live demand wants and hasn't
    /// started is dropped; what runs goes on (it may be cancelled later for live work).
    fn withdraw(&mut self, o: usize, by: usize) {
        let Some(rs) = self.runs.get_mut(o) else { return };
        if rs.withdrawn || rs.end != RunEnd::Open {
            return;
        }
        rs.withdrawn = true;
        let held = std::mem::take(&mut rs.held);
        self.note(o, format!("demands withdrawn by run {by}"));
        let jobs = self.runs.get(o).map(|rs| rs.jobs.clone()).unwrap_or_default();
        for (j, st) in jobs.into_iter().enumerate() {
            match st {
                JobSt::Attached(x) if !self.wanted(x) && matches!(self.exec_st.get(x), Some(ExecSt::Queued | ExecSt::Launching)) => {
                    self.note(o, format!("execution {x} dropped before it started: no live demand wants it"));
                    self.stop(x, Stop::Dropped);
                }
                JobSt::Waiting if held => self.set_job(o, j, JobSt::Cancelled),
                _ => {}
            }
        }
        self.check_concluded(o);
    }

    /// Whether a live demand (one not withdrawn) waits on the execution.
    fn wanted(&self, x: usize) -> bool {
        self.serves.get(x).is_some_and(|s| s.iter().any(|(r, j)| self.runs.get(*r).is_some_and(|rs| !rs.withdrawn) && self.job_st(*r, *j) == Some(&JobSt::Attached(x))))
            || self.execs.get(x).is_some_and(|e| e.bisect.is_some())
    }

    fn start_run(&mut self, r: usize) {
        if let Some(rs) = self.runs.get_mut(r) {
            rs.started = true;
        }
    }

    fn apply(&mut self, g: &str, actions: Vec<GroupAction>) {
        for a in actions {
            match a {
                GroupAction::Start(m) => {
                    if let Ok(r) = m.parse::<usize>() {
                        self.start_run(r);
                    }
                }
                GroupAction::Cancel { member, running, reason } => {
                    let Ok(r) = member.parse::<usize>() else { continue };
                    self.note(r, format!("run cancelled: {reason}"));
                    self.cancel_run(r);
                    // Its jobs stop at once, so its place frees now.
                    if running {
                        let more = self.groups.leave(g, &member);
                        self.apply(g, more);
                    }
                }
            }
        }
    }

    fn cancel_run(&mut self, r: usize) {
        let Some(rs) = self.runs.get_mut(r) else { return };
        if rs.end != RunEnd::Open {
            return;
        }
        rs.end = RunEnd::Cancelled;
        let jobs: Vec<JobSt> = rs.jobs.clone();
        for (j, st) in jobs.into_iter().enumerate() {
            match st {
                JobSt::Attached(x) => {
                    self.stop(x, Stop::Superseded);
                    self.set_job(r, j, JobSt::Cancelled);
                }
                JobSt::Waiting => self.set_job(r, j, JobSt::Cancelled),
                _ => {}
            }
        }
    }

    /// Stops an execution: one running stops at once (`why`); one waiting is dropped. The demands it
    /// answered are left without a result.
    fn stop(&mut self, x: usize, why: Stop) {
        let Some(st) = self.exec_st.get(x).copied() else { return };
        match st {
            ExecSt::Queued | ExecSt::Launching => {
                if st == ExecSt::Launching {
                    self.release(x);
                }
                self.queue.retain(|_, v| *v != x);
                if let Some(e) = self.execs.get_mut(x) {
                    e.stop = Some(Stop::Dropped);
                }
            }
            ExecSt::Running => {
                self.release(x);
                self.running.remove(&x);
                let now = self.now;
                if let Some(e) = self.execs.get_mut(x) {
                    e.ended = Some(now);
                    e.stop = Some(why);
                }
            }
            ExecSt::Over => return,
        }
        if let Some(s) = self.exec_st.get_mut(x) {
            *s = ExecSt::Over;
        }
        let key = self.execs.get(x).map(|e| e.key.clone()).unwrap_or_default();
        if self.inflight.get(&key) == Some(&x) {
            self.inflight.remove(&key);
        }
        self.steps_inflight.retain(|_, (y, _)| *y != x);
        let served = self.serves.get(x).cloned().unwrap_or_default();
        for (r, j) in served {
            if self.job_st(r, j) == Some(&JobSt::Attached(x)) {
                self.set_job(r, j, JobSt::Cancelled);
                self.check_concluded(r);
            }
        }
    }

    /// Frees what an execution holds of its pool.
    fn release(&mut self, x: usize) {
        let trace = self.trace;
        let Some(e) = self.execs.get(x) else { return };
        let Some(p) = e.pool else { return };
        let job = trace.runs.get(e.run).and_then(|r| r.jobs.get(e.job));
        let Some(ps) = self.pools.get_mut(p) else { return };
        match trace.pools.get(p).and_then(|s| s.slots) {
            Some(_) => {
                ps.busy = ps.busy.saturating_sub(1);
                if job.is_some_and(|j| j.exclusive) {
                    ps.exclusive = false;
                }
            }
            None => {
                let (c, m) = job.and_then(|j| j.req).unwrap_or(DEFAULT_REQ);
                ps.cpu = ps.cpu.saturating_sub(c);
                ps.mem = ps.mem.saturating_sub(m);
                ps.pods = ps.pods.saturating_sub(1);
            }
        }
    }

    /// Readies the run's jobs whose needs have passed, and skips those whose needs failed.
    fn ready(&mut self, r: usize) {
        let trace = self.trace;
        let Some(run) = trace.runs.get(r) else { return };
        if self.runs.get(r).is_some_and(|rs| rs.held || rs.end != RunEnd::Open) {
            return;
        }
        loop {
            let mut moved = false;
            for (j, job) in run.jobs.iter().enumerate() {
                if self.job_st(r, j) != Some(&JobSt::Waiting) {
                    continue;
                }
                let needs: Vec<Option<&JobSt>> = job.needs.iter().filter_map(|n| run.jobs.iter().position(|o| &o.key == n)).map(|i| self.job_st(r, i)).collect();
                if needs.iter().any(|s| matches!(s, Some(JobSt::Done { end: End::Failed | End::TimedOut, .. } | JobSt::Skipped(_)))) {
                    self.set_job(r, j, JobSt::Skipped(self.now));
                    moved = true;
                } else if needs.iter().any(|s| matches!(s, Some(JobSt::Cancelled))) {
                    self.set_job(r, j, JobSt::Cancelled);
                    moved = true;
                } else if needs.iter().all(|s| matches!(s, Some(JobSt::Done { end: End::Succeeded, .. }))) {
                    if self.demand_mode() {
                        self.demand(r, j);
                    } else {
                        let key = self.key_of(r, j);
                        let x = self.new_exec(r, j, key, 0, None);
                        self.set_job(r, j, JobSt::Attached(x));
                    }
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
        self.check_concluded(r);
    }

    /// A ready demand (P2 and later): reuse a finished result, adopt the execution in flight, or
    /// run; a withdrawn demand only takes what's there.
    fn demand(&mut self, r: usize, j: usize) {
        let key = self.key_of(r, j);
        if let Some((end, _)) = self.ledger.get(&key).cloned() {
            self.note(r, format!("job {j} reuses the finished result of {key}"));
            self.set_job(r, j, JobSt::Done { at: self.now, end, how: How::Reused });
            self.record_result(r, j, end);
            return;
        }
        if let Some(x) = self.inflight.get(&key).copied() {
            self.note(r, format!("job {j} adopts execution {x} ({key})"));
            if let Some(s) = self.serves.get_mut(x) {
                s.push((r, j));
            }
            self.set_job(r, j, JobSt::Attached(x));
            return;
        }
        if self.runs.get(r).is_some_and(|rs| rs.withdrawn) {
            self.set_job(r, j, JobSt::Cancelled);
            return;
        }
        let x = self.new_exec(r, j, key.clone(), 0, None);
        self.inflight.insert(key, x);
        self.set_job(r, j, JobSt::Attached(x));
    }

    fn new_exec(&mut self, r: usize, j: usize, key: String, priority: u8, bisect: Option<usize>) -> usize {
        let x = self.execs.len();
        let runtime = self.draw(r, j).map_or(0, |d| d.duration);
        self.execs.push(Exec { run: r, job: j, key, queued: self.now, started: None, ended: None, end: None, stop: None, pool: None, runtime, credit: 0, salvaged: 0, bisect });
        self.exec_st.push(ExecSt::Queued);
        self.serves.push(if bisect.is_some() { Vec::new() } else { vec![(r, j)] });
        self.seq += 1;
        self.queue.insert((priority, self.now, self.seq), x);
        x
    }

    /// Cadence (P4): an exclusive job, and a job on a Mac, run the newest commit when their
    /// resource frees, and are never cancelled for other work.
    fn on_cadence(&self, job: &trace::Job) -> bool {
        self.policy.cadence && (job.exclusive || job.labels.iter().any(|l| l == "macos"))
    }

    fn dispatch(&mut self) {
        let trace = self.trace;
        // Machines an exclusive job on a cadence waits for take no new work meanwhile (P4).
        let mut draining: BTreeSet<usize> = BTreeSet::new();
        if self.policy.cadence {
            for x in self.queue.values() {
                let Some(e) = self.execs.get(*x) else { continue };
                let Some(job) = self.job_of(e.run, e.job) else { continue };
                if job.exclusive && self.wanted(*x) {
                    for (i, p) in trace.pools.iter().enumerate() {
                        if p.slots.is_some() && carries(p, job) {
                            draining.insert(i);
                        }
                    }
                }
            }
        }
        let waiting: Vec<((u8, i64, u64), usize)> = self.queue.iter().map(|(k, v)| (*k, *v)).collect();
        for (k, x) in waiting {
            let Some(e) = self.execs.get(x) else { continue };
            if !self.runs.get(e.run).is_some_and(|rs| rs.started) && e.bisect.is_none() {
                continue;
            }
            if self.place(x, &draining) || (self.demand_mode() && k.0 == 0 && self.wanted(x) && self.make_room(x, &draining) && self.place(x, &draining)) {
                self.queue.remove(&k);
            }
        }
    }

    /// The pool an execution could take now, if any.
    fn room(&self, x: usize, draining: &BTreeSet<usize>) -> Option<usize> {
        let trace = self.trace;
        let e = self.execs.get(x)?;
        let job = self.job_of(e.run, e.job)?;
        let mut best: Option<(usize, u32, u32)> = None;
        for (i, spec) in trace.pools.iter().enumerate() {
            if !carries(spec, job) {
                continue;
            }
            let Some(ps) = self.pools.get(i) else { continue };
            match spec.slots {
                Some(slots) => {
                    let room = !ps.exclusive && (!draining.contains(&i) || job.exclusive) && if job.exclusive { ps.busy == 0 } else { ps.busy < slots };
                    // The least loaded first, then the smaller name (v1's pick_agent, its room
                    // measured as the share of slots taken).
                    let better = best.is_none_or(|(b, b_busy, b_slots)| {
                        let (mine, theirs) = (u64::from(ps.busy) * u64::from(b_slots), u64::from(b_busy) * u64::from(slots));
                        mine < theirs || (mine == theirs && trace.pools.get(b).is_some_and(|o| spec.name < o.name))
                    });
                    if room && better {
                        best = Some((i, ps.busy, slots));
                    }
                }
                None => {
                    let (c, m) = job.req.unwrap_or(DEFAULT_REQ);
                    let fits = ps.cpu + c <= spec.cpu_milli.unwrap_or(u64::MAX) && ps.mem + m <= spec.mem_mib.unwrap_or(u64::MAX) && ps.pods < spec.pods.unwrap_or(u32::MAX);
                    if fits && best.is_none() {
                        best = Some((i, 0, 0));
                    }
                }
            }
        }
        best.map(|(p, _, _)| p)
    }

    /// Whether every root job of a run would have to wait for capacity now (P4's saturation).
    fn saturated(&self, r: usize) -> bool {
        let trace = self.trace;
        let Some(run) = trace.runs.get(r) else { return false };
        run.jobs.iter().filter(|j| j.needs.is_empty()).any(|job| {
            !trace.pools.iter().enumerate().any(|(i, spec)| {
                carries(spec, job)
                    && self.pools.get(i).is_some_and(|ps| match spec.slots {
                        Some(slots) => !ps.exclusive && if job.exclusive { ps.busy == 0 } else { ps.busy < slots },
                        None => {
                            let (c, m) = job.req.unwrap_or(DEFAULT_REQ);
                            ps.cpu + c <= spec.cpu_milli.unwrap_or(u64::MAX) && ps.mem + m <= spec.mem_mib.unwrap_or(u64::MAX)
                        }
                    })
            })
        })
    }

    /// Puts an execution on a pool if one has room: a machine's slot, or room in the k8s quota.
    fn place(&mut self, x: usize, draining: &BTreeSet<usize>) -> bool {
        let trace = self.trace;
        let Some(p) = self.room(x, draining) else { return false };
        let Some(e) = self.execs.get(x) else { return false };
        let (r, j) = (e.run, e.job);
        let Some(job) = self.job_of(r, j) else { return false };
        let (exclusive, req) = (job.exclusive, job.req.unwrap_or(DEFAULT_REQ));
        let is_quota = trace.pools.get(p).is_some_and(|s| s.slots.is_none());
        if let Some(ps) = self.pools.get_mut(p) {
            if is_quota {
                ps.cpu += req.0;
                ps.mem += req.1;
                ps.pods += 1;
            } else {
                ps.busy += 1;
                if exclusive {
                    ps.exclusive = true;
                }
            }
        }
        if let Some(e) = self.execs.get_mut(x) {
            e.pool = Some(p);
        }
        let delay = if is_quota { self.draw(r, j).map_or(0, |d| d.delay) } else { 0 };
        if delay > 0 {
            if let Some(s) = self.exec_st.get_mut(x) {
                *s = ExecSt::Launching;
            }
            self.at(self.now + delay, Ev::Launched(x));
        } else {
            self.begin(x);
        }
        true
    }

    /// The time an execution is expected to have left, from its job's typical duration.
    fn expected_left(&self, y: usize) -> i64 {
        let Some(e) = self.execs.get(y) else { return 0 };
        let (Some(run), Some(job)) = (self.trace.runs.get(e.run), self.job_of(e.run, e.job)) else { return 0 };
        let typical = self.typical.get(&(run.workflow.clone(), job.key.clone())).copied().unwrap_or(e.runtime);
        (typical - e.credit - e.started.map_or(0, |s| self.now - s)).max(0)
    }

    /// For live work that can't be placed (P2): cancels running work no live demand wants, if
    /// that makes room and each has more than `finish_within` left.
    fn make_room(&mut self, x: usize, draining: &BTreeSet<usize>) -> bool {
        let trace = self.trace;
        let Some(e) = self.execs.get(x) else { return false };
        let Some(job) = self.job_of(e.run, e.job) else { return false };
        for (i, spec) in trace.pools.iter().enumerate() {
            // A machine draining for an exclusive job takes no other work, room or not.
            if !carries(spec, job) || (draining.contains(&i) && !job.exclusive) {
                continue;
            }
            let running: Vec<usize> = self.running.iter().copied().filter(|y| self.execs.get(*y).is_some_and(|o| o.pool == Some(i))).collect();
            let mut preemptible: Vec<(i64, usize)> = running
                .iter()
                .filter(|y| !self.wanted(**y) && self.execs.get(**y).and_then(|o| self.job_of(o.run, o.job)).is_some_and(|oj| !self.on_cadence(oj)))
                .map(|y| (self.expected_left(*y), *y))
                .filter(|(left, _)| *left > self.policy.finish_within)
                .collect();
            preemptible.sort_by(|a, b| b.cmp(a));
            let Some(ps) = self.pools.get(i) else { continue };
            let chosen: Vec<usize> = match spec.slots {
                Some(_) if job.exclusive => {
                    if preemptible.len() == running.len() && !running.is_empty() {
                        preemptible.iter().map(|(_, y)| *y).collect()
                    } else {
                        continue;
                    }
                }
                Some(slots) => {
                    let need = usize::try_from((ps.busy + 1).saturating_sub(slots)).unwrap_or(0);
                    let blocked = ps.exclusive && !preemptible.iter().any(|(_, y)| self.execs.get(*y).and_then(|o| self.job_of(o.run, o.job)).is_some_and(|oj| oj.exclusive));
                    if need == 0 || blocked || preemptible.len() < need {
                        continue;
                    }
                    preemptible.iter().take(need).map(|(_, y)| *y).collect()
                }
                None => {
                    let (c, m) = job.req.unwrap_or(DEFAULT_REQ);
                    let (mut cpu, mut mem) = (ps.cpu, ps.mem);
                    let (cap_c, cap_m) = (spec.cpu_milli.unwrap_or(u64::MAX), spec.mem_mib.unwrap_or(u64::MAX));
                    let mut picked = Vec::new();
                    for (_, y) in &preemptible {
                        if cpu + c <= cap_c && mem + m <= cap_m {
                            break;
                        }
                        let (yc, ym) = self.execs.get(*y).and_then(|o| self.job_of(o.run, o.job)).and_then(|oj| oj.req).unwrap_or(DEFAULT_REQ);
                        cpu = cpu.saturating_sub(yc);
                        mem = mem.saturating_sub(ym);
                        picked.push(*y);
                    }
                    if picked.is_empty() || cpu + c > cap_c || mem + m > cap_m {
                        continue;
                    }
                    picked
                }
            };
            for y in chosen {
                self.preempt(y, x);
            }
            return true;
        }
        false
    }

    /// Cancels unwanted running work for live work; a k8s job's progress is kept for the next run
    /// of its job on its lane (its warm workspace).
    fn preempt(&mut self, y: usize, for_x: usize) {
        let trace = self.trace;
        let Some(e) = self.execs.get(y) else { return };
        let (r, j, credit) = (e.run, e.job, e.credit);
        let progress = credit + e.started.map_or(0, |s| self.now - s);
        let ephemeral = e.pool.and_then(|p| trace.pools.get(p)).is_some_and(|p| p.slots.is_none());
        let left = self.expected_left(y);
        self.note(r, format!("execution {y} cancelled for execution {for_x}: no live demand wants it and it had about {left} s left"));
        self.stop(y, Stop::Unwanted);
        if ephemeral && self.policy.warm_credit > 0.0 {
            let lane = self.lane_of(r);
            let key = self.job_of(r, j).map(|job| job.key.clone()).unwrap_or_default();
            self.warm.insert((lane, key), (progress, y));
        }
    }

    fn launched(&mut self, x: usize) {
        if self.exec_st.get(x) == Some(&ExecSt::Launching) {
            self.begin(x);
        }
    }

    fn begin(&mut self, x: usize) {
        let now = self.now;
        let trace = self.trace;
        let Some(e) = self.execs.get(x) else { return };
        let (r, j) = (e.run, e.job);
        let ephemeral = e.pool.and_then(|p| trace.pools.get(p)).is_some_and(|p| p.slots.is_none());
        let mut runtime = self.draw(r, j).map_or(0, |d| d.duration);
        let mut credit = 0;
        // A warm workspace from the lane's cancelled run of this job (P2 and later).
        if self.demand_mode() && ephemeral && e.bisect.is_none() {
            let lane = self.lane_of(r);
            let key = self.job_of(r, j).map(|job| job.key.clone()).unwrap_or_default();
            if let Some((progress, from)) = self.warm.remove(&(lane, key)) {
                let cap = (runtime as f64 * self.policy.warm_credit) as i64;
                credit = progress.min(cap).max(0);
                runtime -= credit;
                let from_used = self.execs.get(from).map_or(0, Exec::used);
                if let Some(f) = self.execs.get_mut(from) {
                    f.salvaged = credit.min(from_used);
                }
                self.note(r, format!("execution {x} starts on execution {from}'s warm workspace, {credit} s ahead"));
            }
        }
        if self.policy.share.is_some() && self.execs.get(x).is_some_and(|e| e.bisect.is_none()) {
            runtime = self.share(x, r, j, runtime);
            // What it makes for other pipelines can be taken from it while it runs.
            if let Some(run) = trace.runs.get(r)
                && let Some(job) = run.jobs.get(j)
            {
                for (wf, jk, step, class) in PRODUCES {
                    if run.workflow == *wf
                        && job.key == *jk
                        && let Some(k) = trace.commits.get(run.commit).and_then(|c| c.keys.get(*class))
                    {
                        let sk = ((*step).to_owned(), k.clone());
                        if !self.steps_done.contains_key(&sk) && !self.steps_inflight.contains_key(&sk) {
                            self.steps_inflight.insert(sk, (x, now + runtime));
                        }
                    }
                }
            }
        }
        if let Some(e) = self.execs.get_mut(x) {
            e.started = Some(now);
            e.runtime = runtime;
            e.credit = credit;
        }
        if let Some(s) = self.exec_st.get_mut(x) {
            *s = ExecSt::Running;
        }
        self.running.insert(x);
        self.at(now + runtime, Ev::Finish(x));
    }

    /// P5: the steps another pipeline's work already made, or is making, are taken from it; what
    /// no one has made yet, this execution makes for the others.
    fn share(&mut self, x: usize, r: usize, j: usize, mut runtime: i64) -> i64 {
        let trace = self.trace;
        let now = self.now;
        let Some(run) = trace.runs.get(r) else { return runtime };
        let keys = trace.commits.get(run.commit).map(|c| c.keys.clone()).unwrap_or_default();
        let mut steps: Vec<(String, i64)> = self.draw(r, j).map(|d| d.shared.iter().map(|(k, v)| (k.clone(), *v)).collect()).unwrap_or_default();
        if run.workflow == "image" {
            let compile = (runtime as f64 * self.policy.share.unwrap_or(0.0)) as i64;
            if compile > 0 {
                steps.push(("musl".into(), compile));
            }
        }
        let mut offset = 0;
        for (step, took) in steps {
            let Some(k) = keys.get(step_class(&step)) else { continue };
            let sk = (step.clone(), k.clone());
            if self.steps_done.get(&sk).is_some_and(|t| *t <= now) {
                runtime -= took;
                self.note(r, format!("execution {x} takes step {step} from work already done"));
            } else if let Some((y, at)) = self.steps_inflight.get(&sk).copied().filter(|(y, _)| *y != x) {
                runtime = (runtime - took).max(at - now);
                self.note(r, format!("execution {x} takes step {step} from execution {y}, ready at {at}"));
            } else if step != "musl" {
                offset += took;
                self.steps_inflight.insert(sk, (x, now + offset));
            }
        }
        runtime.max(0)
    }

    fn finish(&mut self, x: usize) {
        if self.exec_st.get(x) != Some(&ExecSt::Running) {
            return;
        }
        let trace = self.trace;
        let Some(e) = self.execs.get(x) else { return };
        let (r, j, key, bisect) = (e.run, e.job, e.key.clone(), e.bisect);
        let draw = self.draw(r, j).cloned();
        let end = draw.as_ref().map_or(End::Succeeded, |d| d.end);
        self.release(x);
        self.running.remove(&x);
        let now = self.now;
        if let Some(e) = self.execs.get_mut(x) {
            e.ended = Some(now);
            e.end = Some(end);
        }
        if let Some(s) = self.exec_st.get_mut(x) {
            *s = ExecSt::Over;
        }
        if let Some(b) = bisect {
            self.bisect_step(b);
            return;
        }
        if self.inflight.get(&key) == Some(&x) {
            self.inflight.remove(&key);
        }
        // What it made for other pipelines (P5).
        let done: Vec<(String, String)> = self.steps_inflight.iter().filter(|(_, (y, _))| *y == x).map(|(k, _)| k.clone()).collect();
        for k in done {
            self.steps_inflight.remove(&k);
            self.steps_done.insert(k, now);
        }
        if let Some(run) = trace.runs.get(r)
            && let Some(job) = run.jobs.get(j)
        {
            for (wf, jk, step, class) in PRODUCES {
                if run.workflow == *wf
                    && job.key == *jk
                    && let Some(k) = trace.commits.get(run.commit).and_then(|c| c.keys.get(*class))
                {
                    self.steps_done.insert(((*step).into(), k.clone()), now);
                }
            }
        }
        // A test failure is the same for the same inputs; a timeout isn't kept.
        if self.demand_mode() && end != End::TimedOut {
            self.ledger.insert(key, (end, draw.and_then(|d| d.failed_step)));
        }
        let served = self.serves.get(x).cloned().unwrap_or_default();
        for (sr, sj) in &served {
            if self.job_st(*sr, *sj) == Some(&JobSt::Attached(x)) {
                let how = if (*sr, *sj) == (r, j) { How::Ran } else { How::Adopted };
                self.set_job(*sr, *sj, JobSt::Done { at: now, end, how });
                self.record_result(*sr, *sj, end);
            }
        }
        for (sr, _) in served {
            self.ready(sr);
        }
    }

    /// Keeps a job's result for its lane (P3): a new failure starts a bisection over the commits
    /// it may have come from.
    fn record_result(&mut self, r: usize, j: usize, end: End) {
        if !self.policy.verdicts {
            return;
        }
        let lane = self.lane_of(r);
        let Some(key) = self.job_of(r, j).map(|job| job.key.clone()) else { return };
        let step = if end.passed() { None } else { self.draw(r, j).and_then(|d| d.failed_step.clone()) };
        let lk = (lane.clone(), key.clone());
        let list = self.results.entry(lk.clone()).or_default();
        let base = list.iter().filter(|(o, _, _)| *o < r).max_by_key(|(o, _, _)| *o).cloned();
        list.push((r, end, step.clone()));
        if end.passed() {
            return;
        }
        if let Some((_, b_end, b_step)) = &base
            && !b_end.passed()
            && *b_step == step
        {
            return;
        }
        let from = base.map_or(0, |(o, _, _)| o + 1);
        let trace = self.trace;
        let suspects: Vec<usize> = self
            .lanes
            .get(&lane)
            .map(|v| v.iter().copied().filter(|o| *o >= from && *o < r).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter(|o| trace.runs.get(*o).is_some_and(|run| run.jobs.iter().any(|oj| oj.key == key)))
            .filter(|o| !self.results.get(&lk).is_some_and(|l| l.iter().any(|(q, _, _)| q == o)))
            .filter(|o| !self.in_bisect.contains(&(lane.clone(), key.clone(), *o)))
            .collect();
        if suspects.is_empty() {
            return;
        }
        let n = u32::try_from(suspects.len()).unwrap_or(u32::MAX);
        let steps = u32::BITS - n.leading_zeros();
        for o in &suspects {
            self.in_bisect.insert((lane.clone(), key.clone(), *o));
        }
        let b = self.bisections.len();
        self.note(r, format!("job {j} newly failed: bisecting {} commits in {steps} runs", suspects.len()));
        self.bisections.push(Bisection { lane, key, suspects, done: None });
        self.bisect_st.push(BisectSt { run: r, job: j, left: steps });
        let k = format!("bisect{b}");
        self.new_exec(r, j, k, 1, Some(b));
    }

    /// One bisection run finished: the next, or the culprit named.
    fn bisect_step(&mut self, b: usize) {
        let Some(st) = self.bisect_st.get_mut(b) else { return };
        st.left = st.left.saturating_sub(1);
        let (left, r, j) = (st.left, st.run, st.job);
        if left == 0 {
            let now = self.now;
            if let Some(bi) = self.bisections.get_mut(b) {
                bi.done = Some(now);
            }
        } else {
            self.new_exec(r, j, format!("bisect{b}"), 1, Some(b));
        }
    }

    fn check_concluded(&mut self, r: usize) {
        let Some(rs) = self.runs.get(r) else { return };
        if rs.end != RunEnd::Open {
            return;
        }
        if rs.jobs.iter().all(|s| matches!(s, JobSt::Done { .. } | JobSt::Skipped(_) | JobSt::Cancelled)) {
            if rs.jobs.contains(&JobSt::Cancelled) {
                if let Some(rs) = self.runs.get_mut(r) {
                    rs.end = RunEnd::Cancelled;
                }
                return;
            }
            let passed = rs.jobs.iter().all(|s| matches!(s, JobSt::Done { end: End::Succeeded, .. }));
            self.conclude(r, passed);
        }
    }

    fn conclude(&mut self, r: usize, passed: bool) {
        let now = self.now;
        let Some(rs) = self.runs.get_mut(r) else { return };
        rs.end = if passed { RunEnd::Passed } else { RunEnd::Failed };
        rs.concluded = Some(now);
        if let Some(g) = rs.joined.clone() {
            let more = self.groups.leave(&g, &r.to_string());
            self.apply(&g, more);
        }
    }
}

/// Whether a pool carries every label a job asks for.
fn carries(p: &trace::Pool, job: &trace::Job) -> bool {
    job.labels.iter().all(|l| p.labels.iter().any(|pl| pl.eq_ignore_ascii_case(l)))
}

impl Outcome {
    /// Runner time used and runner time on jobs stopped as superseded, over every workflow, as
    /// runs_analyze.py counts them (jobs that ended inside the window).
    pub fn runner_time(&self) -> (i64, i64) {
        let used = self.execs.iter().map(Exec::used).sum();
        let superseded = self.execs.iter().filter(|e| e.stop == Some(Stop::Superseded)).map(Exec::used).sum();
        (used, superseded)
    }

    /// Runner time on work no one used (superseded, or unwanted less what a successor reused).
    pub fn wasted(&self) -> i64 {
        self.execs.iter().map(Exec::wasted).sum()
    }

    /// The share of runner time on jobs stopped as superseded.
    pub fn superseded_share(&self) -> f64 {
        let (used, superseded) = self.runner_time();
        if used == 0 { 0.0 } else { superseded as f64 / used as f64 }
    }

    /// How many of `workflow`'s pushes to main concluded, of how many.
    pub fn concluded(&self, trace: &Trace, workflow: &str) -> (usize, usize) {
        let mut n = 0;
        let mut done = 0;
        for (r, out) in trace.runs.iter().zip(&self.runs) {
            if r.workflow == workflow && r.branch == "main" && r.is_push() {
                n += 1;
                if matches!(out.end, RunEnd::Passed | RunEnd::Failed) {
                    done += 1;
                }
            }
        }
        (done, n)
    }
}
