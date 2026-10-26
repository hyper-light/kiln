//! The land queue, replayed over the recorded week (docs/ci.md W28-R35 to R37, AC31 to AC33; kiln
//! plan `01a1751638e773b98b5092662fc06bf5`, step 2). The week's ci pushes to main are the changes
//! that wanted to land: each enqueues as its run was created and needs its ci pipeline green on
//! the tree it would land as. Both policies meet the same changes, pools and draws as
//! [`super::replay`], and the same job on the same tree ends the same way, whoever runs it.
//!
//! - **Serial**, today's queue (`crates/kiln/src/ci/land.rs`): one candidate at a time, up to
//!   `batch` changes from the queue's front tested together by the newest one's whole pipeline,
//!   nothing reused; a failure halves the next batch until the culprit is alone and ejected
//!   ([`crate::land::next_batch`]).
//! - **Components**, step 9: [`crate::land::conflict`]'s graph over the changes pending, each
//!   change's changed input classes standing for the units it affects (the trace keys work by
//!   class, not by file). Each component tests the first changes of its queue at once, as many as
//!   its AIMD window allows, each on its expected stack. A job's result is keyed by the job and the
//!   tree its class has on that stack, so a key the trunk or any candidate already has answers it
//!   and one in flight is shared. A speculative candidate starts only while the bottleneck its
//!   check needs is below the cap, counting what runs there and what waits for it, the shortest
//!   expected service first. Every component's work waits in one pooled queue ranked by
//!   [`crate::land::order`]: the critical path first, then the shortest expected service, aged. An
//!   ejection re-keys the candidates behind it: what still keys keeps its result, the rest runs
//!   again.
//!
//! Pure: no clock, disk or randomness beyond the seed; ordered maps throughout.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::insights::{Node, critical_path};
use crate::land::conflict::{Change, Conflicts, Decision, Unit, Units};
use crate::land::next_batch;
use crate::land::order::{Order, Role, Speculation, Waiting};
use crate::sched::admit;

use super::draw::Draw;
use super::trace::{self, End, Trace};

#[cfg(test)]
mod tests;

/// How the queue runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    /// Today's: serial batch-and-bisect.
    Serial { batch: u32 },
    /// Step 9's: components side by side, one pooled queue, AIMD windows, capped admission.
    Components(Order),
}

/// A land-queue replay policy.
#[derive(Debug, Clone, PartialEq)]
pub struct LandPolicy {
    pub name: String,
    pub mode: Mode,
}

impl LandPolicy {
    /// Today's queue testing up to `batch` changes at once (kiln's own `.kiln/review.yaml` sets no
    /// `queue.batch`, so kiln's is the default, one).
    pub fn serial(batch: u32) -> LandPolicy {
        LandPolicy { name: format!("serial, batch {batch}"), mode: Mode::Serial { batch } }
    }

    /// Step 9's queue under `order`.
    pub fn components(order: Order) -> LandPolicy {
        LandPolicy { name: "components".into(), mode: Mode::Components(order) }
    }

    /// The report's two: today's queue as kiln runs it, and step 9's with its defaults.
    pub fn pair() -> Vec<LandPolicy> {
        vec![LandPolicy::serial(1), LandPolicy::components(Order::default())]
    }
}

/// One job of a change's check, as the queue reads it from the change's recorded run.
#[derive(Debug, Clone)]
struct Spec {
    /// Its key in the run (`rust`, `ui`).
    key: String,
    /// The input class its tree is keyed by.
    class: String,
    /// What it needs, as places in the check.
    needs: Vec<usize>,
    /// Its expected service: the median of the trace's completed runs of it.
    typical: i64,
    /// Whether it lies on its check's critical path, by expected service.
    critical: bool,
    /// The change and the place in its run it comes from: its draw and its labels.
    src: (usize, usize),
}

/// A change in the queue.
#[derive(Debug, Clone)]
struct Item {
    /// Its run's place in the trace.
    run: usize,
    at: i64,
    /// The input classes its commit changed from the previous push's: the units it affects.
    aff: BTreeSet<String>,
    /// Its check: its recorded run's jobs.
    check: Vec<Spec>,
    /// Its check's expected service: its critical path, by the jobs' expected services.
    service: i64,
}

/// How one job of a candidate stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobSt {
    /// Not answered yet: its needs haven't all passed.
    Queued,
    /// Waiting on the execution.
    Attached(usize),
    Done(End),
    /// A need of it failed or was skipped.
    Skipped,
}

/// One candidate: a change's check on the stack it would land on.
#[derive(Debug, Clone)]
struct Cand {
    /// The change it checks (serial: its batch's newest).
    item: usize,
    /// The changes it lands, in queue order (components: the change alone).
    batch: Vec<usize>,
    jobs: Vec<JobSt>,
    /// Each job's work key, as it resolved.
    keys: Vec<Option<String>>,
    decided: bool,
    /// Its check: its change's jobs, or a batch's every change's (the newest's where two share a
    /// job), its critical path recomputed over them.
    specs: Vec<Spec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecSt {
    Queued,
    /// Placed on the k8s quota, its pod not started yet.
    Launching,
    Running,
    Over,
}

/// One execution of one job on one tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Exec {
    /// The change whose demand made it, and the job's place in its check.
    pub item: usize,
    pub job: usize,
    /// Its work key: the job, and the tree its class has.
    pub key: String,
    pub queued: i64,
    /// When it took a runner, or room in the k8s quota.
    pub placed: Option<i64>,
    pub started: Option<i64>,
    pub ended: Option<i64>,
    pub end: Option<End>,
    /// Stopped before its end: no candidate wanted its result any more.
    pub stale: bool,
    pub pool: Option<usize>,
    pub runtime: i64,
    /// On its component's critical path as it was made (W28-R35).
    pub critical: bool,
    /// How the job ends on this tree.
    pub outcome: End,
}

impl Exec {
    /// Runner time it used.
    pub fn used(&self) -> i64 {
        match (self.started, self.ended) {
            (Some(s), Some(e)) if e >= s => e - s,
            _ => 0,
        }
    }

    /// How long it waited for a runner: until it was placed, else until it stopped or `end`.
    pub fn waited(&self, end: i64) -> i64 {
        self.placed.or(self.ended).unwrap_or(end) - self.queued
    }
}

/// What one change did in the queue.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemOut {
    /// When it enqueued.
    pub at: i64,
    /// When work for it first ran: its candidate's (under serial, its batch's) first job.
    pub started: Option<i64>,
    pub landed: Option<i64>,
    pub ejected: bool,
}

/// What the queue did under one policy and seed.
#[derive(Debug, Clone, PartialEq)]
pub struct LandOut {
    pub policy: String,
    pub seed: u64,
    pub items: Vec<ItemOut>,
    pub execs: Vec<Exec>,
    /// Each candidate's changes, in the order the candidates were made.
    pub candidates: Vec<Vec<usize>>,
    /// The widest any component's window was, and the cap (components).
    pub window_max: u32,
    pub window_cap: u32,
    /// The order's aging bound (components).
    pub aging_bound: i64,
    /// When the trace ends.
    pub end: i64,
    /// What the queue decided and why, in order.
    pub decisions: Vec<String>,
}

impl LandOut {
    /// The landed changes' mean time from enqueue to landed (seconds).
    pub fn mean_to_land(&self) -> f64 {
        let (mut sum, mut n) = (0i64, 0usize);
        for o in &self.items {
            if let Some(l) = o.landed {
                sum += l - o.at;
                n += 1;
            }
        }
        if n == 0 { 0.0 } else { sum as f64 / n as f64 }
    }

    /// Runner-seconds used.
    pub fn used(&self) -> i64 {
        self.execs.iter().map(Exec::used).sum()
    }

    /// How many changes landed, and how many were ejected.
    pub fn fates(&self) -> (usize, usize) {
        (self.items.iter().filter(|o| o.landed.is_some()).count(), self.items.iter().filter(|o| o.ejected).count())
    }

    /// Runner-hours per landed change.
    pub fn runner_hours_per_landed(&self) -> f64 {
        let (landed, _) = self.fates();
        if landed == 0 { 0.0 } else { self.used() as f64 / 3600.0 / landed as f64 }
    }

    /// The longest any execution waited for a runner (seconds).
    pub fn max_wait(&self) -> i64 {
        self.execs.iter().map(|e| e.waited(self.end)).max().unwrap_or(0)
    }
}

#[derive(Debug, Clone, Default)]
struct PoolSt {
    busy: u32,
    cpu: u64,
    mem: u64,
    pods: u32,
}

/// At one time, completions free capacity before launches and arrivals take it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ev {
    Finish(usize),
    Launched(usize),
    Arrive(usize),
    /// A held batch's time to start.
    Wake,
}

impl Ev {
    fn order(self) -> u8 {
        match self {
            Ev::Finish(_) => 0,
            Ev::Launched(_) => 1,
            Ev::Arrive(_) => 2,
            Ev::Wake => 3,
        }
    }
}

/// A change's part in a tree's key: tree keys are sets of changes, so the trunk's part is kept as
/// a count and a sum of these, and a change moving from a stack to the trunk keeps every key.
fn mix(i: usize) -> u64 {
    let mut z = (i as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Each job's typical duration: the median of its completed runs in the trace.
fn typical(trace: &Trace) -> BTreeMap<(String, String), i64> {
    let mut took: BTreeMap<(String, String), Vec<i64>> = BTreeMap::new();
    for r in &trace.runs {
        for j in &r.jobs {
            if let Some((t, _)) = j.completed() {
                took.entry((r.workflow.clone(), j.key.clone())).or_default().push(t);
            }
        }
    }
    took.into_iter()
        .map(|(k, mut v)| {
            v.sort_unstable();
            let m = v.get(v.len() / 2).copied().unwrap_or(0);
            (k, m)
        })
        .collect()
}

/// The changes: ci's pushes to main, in the order they enqueued. A change affects the input
/// classes whose keys its commit changed from the previous push's (all of them, for the first).
fn items(trace: &Trace, typical: &BTreeMap<(String, String), i64>) -> Vec<Item> {
    let mut runs: Vec<usize> = trace.runs.iter().enumerate().filter(|(_, r)| r.workflow == "ci" && r.branch == "main" && r.is_push()).map(|(i, _)| i).collect();
    runs.sort_by_key(|&r| (trace.runs.get(r).map_or(0, |x| x.at), r));
    let mut prev: Option<&BTreeMap<String, String>> = None;
    let mut out = Vec::with_capacity(runs.len());
    for r in runs {
        let Some(run) = trace.runs.get(r) else { continue };
        let keys = trace.commits.get(run.commit).map(|c| &c.keys);
        let aff: BTreeSet<String> = keys.map(|k| k.iter().filter(|(c, v)| prev.is_none_or(|p| p.get(*c) != Some(*v))).map(|(c, _)| c.clone()).collect()).unwrap_or_default();
        if keys.is_some() {
            prev = keys;
        }
        let typ = |j: &trace::Job| typical.get(&(run.workflow.clone(), j.key.clone())).copied().unwrap_or(60);
        let nodes: Vec<Node> = run.jobs.iter().map(|j| Node { id: j.key.clone(), needs: j.needs.clone(), duration: typ(j) as f64 }).collect();
        let path: BTreeSet<String> = critical_path(&nodes).into_iter().collect();
        let service = run.jobs.iter().filter(|j| path.contains(&j.key)).map(typ).sum();
        let idx = out.len();
        let check = run
            .jobs
            .iter()
            .enumerate()
            .map(|(n, j)| Spec {
                key: j.key.clone(),
                class: j.class.clone().unwrap_or_else(|| run.workflow.clone()),
                needs: j.needs.iter().filter_map(|n| run.jobs.iter().position(|o| &o.key == n)).collect(),
                typical: typ(j),
                critical: path.contains(&j.key),
                src: (idx, n),
            })
            .collect();
        out.push(Item { run: r, at: run.at, aff, check, service });
    }
    out
}

/// Replays the week's ci pushes to main through the land queue under `policy`.
pub fn replay(trace: &Trace, policy: &LandPolicy, seed: u64) -> LandOut {
    let typical = typical(trace);
    let items = items(trace, &typical);
    // The conflict graph's units: one per input class, a change's files in the folders of the
    // classes it changes.
    let classes: BTreeSet<String> = items.iter().flat_map(|it| it.aff.iter().cloned()).collect();
    let units = Units::new(classes.iter().map(|c| Unit { name: c.clone(), ecosystem: "class".into(), dir: c.clone(), ..Unit::default() }));
    let (serial, order) = match &policy.mode {
        Mode::Serial { batch } => (Some(*batch), Order::default()),
        Mode::Components(o) => (None, *o),
    };
    let out = items.iter().map(|it| ItemOut { at: it.at, ..ItemOut::default() }).collect();
    let mut e = Engine {
        trace,
        serial,
        order,
        units,
        items,
        draws: super::draw::draws(trace, seed),
        now: 0,
        seq: 0,
        events: BTreeMap::new(),
        pools: vec![PoolSt::default(); trace.pools.len()],
        execs: Vec::new(),
        st: Vec::new(),
        serves: Vec::new(),
        waiting: BTreeSet::new(),
        eligible: BTreeMap::new(),
        deferred: BTreeSet::new(),
        orphans: BTreeSet::new(),
        ledger: BTreeMap::new(),
        inflight: BTreeMap::new(),
        cands: Vec::new(),
        out,
        pending: Vec::new(),
        cand_of: BTreeMap::new(),
        passed: BTreeSet::new(),
        comps: Vec::new(),
        comp_of: BTreeMap::new(),
        windows: BTreeMap::new(),
        trunk: BTreeMap::new(),
        told: BTreeSet::new(),
        limit: BTreeMap::new(),
        wakes: BTreeSet::new(),
        active: None,
        failures: 0,
        window_max: 0,
        decisions: Vec::new(),
    };
    let arrivals: Vec<(usize, i64)> = e.items.iter().enumerate().map(|(i, it)| (i, it.at)).collect();
    for (i, at) in arrivals {
        e.at(at, Ev::Arrive(i));
    }
    e.run();
    LandOut {
        policy: policy.name.clone(),
        seed,
        items: e.out,
        candidates: e.cands.iter().map(|c| c.batch.clone()).collect(),
        execs: e.execs,
        window_max: e.window_max,
        window_cap: Speculation::new(order.window).cap(),
        aging_bound: order.aging_bound(),
        end: trace.end,
        decisions: e.decisions,
    }
}

struct Engine<'a> {
    trace: &'a Trace,
    /// The batch, under serial.
    serial: Option<u32>,
    order: Order,
    units: Units,
    items: Vec<Item>,
    draws: Vec<Vec<Draw>>,
    now: i64,
    seq: u64,
    events: BTreeMap<(i64, u8, u64), Ev>,
    pools: Vec<PoolSt>,
    execs: Vec<Exec>,
    st: Vec<ExecSt>,
    /// The demands each execution answers: (candidate, job).
    serves: Vec<BTreeSet<(usize, usize)>>,
    /// Executions waiting for a runner.
    waiting: BTreeSet<usize>,
    /// When each change in its component's window without a candidate came into the window.
    eligible: BTreeMap<usize, i64>,
    /// Changes whose candidates are deferred, the decisions having said so.
    deferred: BTreeSet<usize>,
    /// Executions that lost their last demand; stopped once the queue settles unless one returns.
    orphans: BTreeSet<usize>,
    /// How each job ended on each tree, by work key: under components, results to reuse; under
    /// serial, only how a job on a tree ends when it runs again.
    ledger: BTreeMap<String, End>,
    /// Executions in flight, by work key (components).
    inflight: BTreeMap<String, usize>,
    cands: Vec<Cand>,
    out: Vec<ItemOut>,
    /// Changes enqueued that neither landed nor were ejected, in queue order.
    pending: Vec<usize>,
    /// Each pending change's live candidate (components).
    cand_of: BTreeMap<usize, usize>,
    /// Pending changes whose candidates passed, waiting for those ahead of them.
    passed: BTreeSet<usize>,
    /// The conflict graph's components over `pending`, each in queue order.
    comps: Vec<Vec<usize>>,
    comp_of: BTreeMap<usize, usize>,
    /// Each pending change's component's window: every member holds the same.
    windows: BTreeMap<usize, Speculation>,
    /// The trunk's landed changes of each class: how many, and the sum of their [`mix`].
    trunk: BTreeMap<String, (usize, u64)>,
    /// Changes whose verdict the decisions have.
    told: BTreeSet<usize>,
    /// Batches: how many changes a component's next batch may hold, by its head, after a failure
    /// narrowed the suspects.
    limit: BTreeMap<usize, usize>,
    /// The times a held batch wakes the queue.
    wakes: BTreeSet<i64>,
    /// Serial: the candidate under test, and failures since the last land.
    active: Option<usize>,
    failures: u32,
    window_max: u32,
    decisions: Vec<String>,
}

impl Engine<'_> {
    fn at(&mut self, t: i64, ev: Ev) {
        self.seq += 1;
        self.events.insert((t, ev.order(), self.seq), ev);
    }

    fn note(&mut self, what: String) {
        self.decisions.push(format!("{}: {what}", self.now));
    }

    fn run(&mut self) {
        while let Some(t) = self.events.keys().next().map(|k| k.0) {
            if t > self.trace.end {
                break;
            }
            self.now = t;
            while let Some(entry) = self.events.first_entry() {
                if entry.key().0 != t {
                    break;
                }
                match entry.remove() {
                    Ev::Arrive(i) => self.pending.push(i),
                    Ev::Wake => {}
                    Ev::Launched(x) => self.launched(x),
                    Ev::Finish(x) => self.finish(x),
                }
            }
            if self.serial.is_some() {
                self.settle_serial();
            } else {
                self.settle_components();
            }
            self.dispatch();
        }
    }

    fn draw(&self, i: usize, j: usize) -> Option<&Draw> {
        self.items.get(i).and_then(|it| self.draws.get(it.run)).and_then(|d| d.get(j))
    }

    fn trace_job(&self, i: usize, j: usize) -> Option<&trace::Job> {
        self.items.get(i).and_then(|it| self.trace.runs.get(it.run)).and_then(|r| r.jobs.get(j))
    }

    fn set(&mut self, c: usize, j: usize, st: JobSt) {
        if let Some(slot) = self.cands.get_mut(c).and_then(|cd| cd.jobs.get_mut(j)) {
            *slot = st;
        }
    }

    fn new_cand(&mut self, item: usize, batch: Vec<usize>) -> usize {
        let specs = if self.order.batch && self.serial.is_none() { self.union(&batch) } else { self.items.get(item).map(|it| it.check.clone()).unwrap_or_default() };
        let n = specs.len();
        self.cands.push(Cand { item, batch, jobs: vec![JobSt::Queued; n], keys: vec![None; n], decided: false, specs });
        self.cands.len() - 1
    }

    /// A batch's check (W28-R36): every job of every change in it, the newest change's where two
    /// have the same, so each change's own checks run on the batch's stack; its critical path is
    /// the longest chain among them.
    fn union(&self, batch: &[usize]) -> Vec<Spec> {
        let mut picked: Vec<(Spec, Vec<String>)> = Vec::new();
        for &m in batch.iter().rev() {
            let Some(it) = self.items.get(m) else { continue };
            for sp in &it.check {
                if picked.iter().all(|(o, _)| o.key != sp.key) {
                    let needs = sp.needs.iter().filter_map(|n| it.check.get(*n).map(|d| d.key.clone())).collect();
                    picked.push((sp.clone(), needs));
                }
            }
        }
        let keys: Vec<String> = picked.iter().map(|(sp, _)| sp.key.clone()).collect();
        let mut out: Vec<Spec> = picked
            .into_iter()
            .map(|(mut sp, needs)| {
                sp.needs = needs.iter().filter_map(|k| keys.iter().position(|o| o == k)).collect();
                sp
            })
            .collect();
        let nodes: Vec<Node> = out.iter().map(|sp| Node { id: sp.key.clone(), needs: sp.needs.iter().filter_map(|n| out.get(*n).map(|d| d.key.clone())).collect(), duration: sp.typical as f64 }).collect();
        let path: BTreeSet<String> = critical_path(&nodes).into_iter().collect();
        for sp in &mut out {
            sp.critical = path.contains(&sp.key);
        }
        out
    }

    /// The tree class `class` has on `stack` (the changes over the trunk, in queue order): the
    /// trunk's changes of it and the stack's, as a set, since changes that conflict land in queue
    /// order and those that don't commute (W28-R33).
    fn tree(&self, class: &str, stack: &[usize]) -> String {
        let (mut n, mut h) = self.trunk.get(class).copied().unwrap_or((0, 0));
        for &x in stack {
            if self.items.get(x).is_some_and(|it| it.aff.contains(class)) {
                n += 1;
                h = h.wrapping_add(mix(x));
            }
        }
        format!("{class}:{n}:{h:016x}")
    }

    /// The stack change `i` is tested on: the changes ahead of it in its component, then itself.
    fn stack(&self, i: usize) -> Vec<usize> {
        let Some(comp) = self.comp_of.get(&i).and_then(|k| self.comps.get(*k)) else { return vec![i] };
        let mut s: Vec<usize> = comp.iter().copied().take_while(|&x| x != i).collect();
        s.push(i);
        s
    }

    fn is_head(&self, i: usize) -> bool {
        self.comp_of.get(&i).and_then(|k| self.comps.get(*k)).and_then(|c| c.first()) == Some(&i)
    }

    // ------------------------------------------------------------------ serial batch-and-bisect

    /// Today's queue: with nothing under test, the next batch from the queue's front (halved
    /// after each failure since the last land); a passing batch lands, a failing one splits, and a
    /// failing lone change is ejected.
    fn settle_serial(&mut self) {
        let batch_size = self.serial.unwrap_or(1);
        for _ in 0..(4 * self.items.len() + 16) {
            let Some(c) = self.active else {
                let n = usize::try_from(next_batch(batch_size, self.failures)).unwrap_or(1);
                let batch: Vec<usize> = self.pending.iter().copied().take(n).collect();
                let Some(&top) = batch.last() else { return };
                let c = self.new_cand(top, batch.clone());
                self.active = Some(c);
                self.note(format!("candidate {c}: {} change(s) on the trunk, checked by #{top}'s whole pipeline", batch.len()));
                continue;
            };
            self.drive(c);
            let Some(cd) = self.cands.get(c) else {
                self.active = None;
                continue;
            };
            if !cd.jobs.iter().all(|s| matches!(s, JobSt::Done(_) | JobSt::Skipped)) {
                return;
            }
            let ok = cd.jobs.iter().all(|s| matches!(s, JobSt::Done(e) if e.passed()));
            let batch = cd.batch.clone();
            self.active = None;
            if ok {
                self.failures = 0;
                for i in batch {
                    self.land(i);
                }
            } else if batch.len() <= 1 {
                // The culprit, alone: out, and the rest tested whole again.
                self.failures = 0;
                for i in batch {
                    self.note(format!("#{i} ejected: its candidate failed alone"));
                    self.eject(i);
                }
            } else {
                self.failures += 1;
                self.note(format!("candidate {c} failed: the next batch is half as big ({} failures since a land)", self.failures));
            }
        }
    }

    // ------------------------------------------------------------------ components

    /// Step 9's queue, until it stands still: the conflict graph over what's pending, candidates
    /// re-keyed to their stacks, each window's candidates made, their jobs answered, then what
    /// passed or failed decided.
    fn settle_components(&mut self) {
        let rounds = 4 * self.items.len() + 16;
        let mut still = false;
        for _ in 0..rounds {
            self.regroup();
            self.rekey();
            self.admit();
            let live: Vec<usize> = self.cand_of.values().copied().collect();
            for c in live {
                self.drive(c);
            }
            if !self.decide() {
                still = true;
                break;
            }
        }
        if !still {
            self.note(format!("the queue didn't settle in {rounds} rounds"));
        }
        let orphans: Vec<usize> = std::mem::take(&mut self.orphans).into_iter().filter(|x| self.serves.get(*x).is_some_and(BTreeSet::is_empty)).collect();
        for x in orphans {
            self.cancel(x);
        }
    }

    /// The conflict graph over the pending changes (W28-R31): its components, each keeping the
    /// window of its oldest change that had one; and each new change's verdict (W28-R34).
    fn regroup(&mut self) {
        let changes: Vec<Change> = self
            .pending
            .iter()
            .map(|&i| Change { id: format!("#{i}"), files: self.items.get(i).map(|it| it.aff.iter().map(|c| format!("{c}/#{i}")).collect()).unwrap_or_default(), targets: BTreeSet::new() })
            .collect();
        let g = Conflicts::new(&self.units, &changes);
        let mut comps: Vec<Vec<usize>> = vec![Vec::new(); g.components.len()];
        for (p, ch) in g.changes.iter().enumerate() {
            if let (Some(k), Some(&i)) = (comps.get_mut(ch.component), self.pending.get(p)) {
                k.push(i);
            }
        }
        let mut windows = BTreeMap::new();
        let mut comp_of = BTreeMap::new();
        for (k, comp) in comps.iter().enumerate() {
            let w = comp.iter().find_map(|i| self.windows.get(i).copied()).unwrap_or_else(|| Speculation::new(self.order.window));
            self.window_max = self.window_max.max(w.max);
            for &i in comp {
                windows.insert(i, w);
                comp_of.insert(i, k);
            }
        }
        let fresh: Vec<usize> = self.pending.iter().copied().filter(|i| !self.told.contains(i)).collect();
        for i in fresh {
            self.told.insert(i);
            let why = g.verdict(&format!("#{i}")).map_or_else(String::new, |v| {
                let waits = v.waits_for();
                match v.decisions.iter().find(|d| matches!(d, Decision::Waits { .. })) {
                    Some(first) if waits.len() > 1 => format!("{first}; and waits for {} more", waits.len() - 1),
                    Some(first) => first.to_string(),
                    None if v.decisions.is_empty() => "lands at once: nothing is ahead of it in the queue".to_owned(),
                    None => "lands beside every change ahead of it: it shares no unit with any".to_owned(),
                }
            });
            self.note(format!("#{i} queued: {why}"));
        }
        self.comps = comps;
        self.comp_of = comp_of;
        self.windows = windows;
    }

    /// Every live candidate's jobs re-keyed to its stack as it stands: a job whose key moved
    /// lets go of what answered it and is asked again; a candidate that had passed opens again.
    fn rekey(&mut self) {
        let live: Vec<(usize, usize)> = self.cand_of.iter().map(|(&i, &c)| (i, c)).collect();
        for (i, c) in live {
            let stack = self.stack(i);
            let specs: Vec<(String, String)> = self.cands.get(c).map(|cd| cd.specs.iter().map(|s| (s.key.clone(), s.class.clone())).collect()).unwrap_or_default();
            let mut moved = false;
            for (j, (key, class)) in specs.iter().enumerate() {
                let Some(Some(was)) = self.cands.get(c).and_then(|cd| cd.keys.get(j)).cloned() else { continue };
                if was == format!("{key}@{}", self.tree(class, &stack)) {
                    continue;
                }
                self.detach(c, j);
                if let Some(k) = self.cands.get_mut(c).and_then(|cd| cd.keys.get_mut(j)) {
                    *k = None;
                }
                self.set(c, j, JobSt::Queued);
                moved = true;
            }
            if moved {
                if let Some(cd) = self.cands.get_mut(c) {
                    for s in cd.jobs.iter_mut().filter(|s| **s == JobSt::Skipped) {
                        *s = JobSt::Queued;
                    }
                    cd.decided = false;
                }
                self.passed.remove(&i);
            }
        }
    }

    /// Each component's window (W28-R36) and admission (W28-R37): its head always has a candidate;
    /// the other changes in the window start theirs while the bottleneck their checks need is
    /// below the cap, the shortest expected service first (aged), each driven before the next is
    /// weighed so what it queues counts; a change past the window is suspended, letting go of what
    /// runs only for it.
    fn admit(&mut self) {
        if self.order.batch {
            for comp in self.comps.clone() {
                self.admit_batch(&comp);
            }
            return;
        }
        for comp in self.comps.clone() {
            let open = comp.first().and_then(|h| self.windows.get(h)).map_or(1, Speculation::open);
            let open = usize::try_from(open).unwrap_or(1);
            for &i in comp.iter().skip(open) {
                self.eligible.remove(&i);
                if let Some(c) = self.cand_of.remove(&i) {
                    self.drop_jobs(c);
                    self.passed.remove(&i);
                    self.note(format!("#{i}: suspended, past its component's window of {open}"));
                }
            }
            let Some(&head) = comp.first() else { continue };
            if !self.cand_of.contains_key(&head) {
                self.start(head, "the head".to_owned());
            }
            let want: Vec<usize> = comp.iter().take(open).skip(1).copied().filter(|i| !self.cand_of.contains_key(i)).collect();
            let now = self.now;
            for &i in &want {
                self.eligible.entry(i).or_insert(now);
            }
            let ws: Vec<Waiting> = want.iter().map(|&i| Waiting { critical: false, service_s: self.items.get(i).map_or(0, |it| it.service), waited_s: now - self.items.get(i).map_or(now, |it| it.at) }).collect();
            for i in self.order.rank(&ws).into_iter().filter_map(|k| want.get(k).copied()) {
                let deferred_s = now - self.eligible.get(&i).copied().unwrap_or(now);
                let a = self.order.admit(Role::Speculative, self.bottleneck(i), deferred_s);
                if !a.runs() {
                    if self.deferred.insert(i) {
                        self.note(format!("#{i}: {a}"));
                    }
                    break;
                }
                let ahead = comp.iter().position(|&x| x == i).unwrap_or(0);
                self.start(i, format!("on its expected stack, {ahead} ahead; {a}"));
            }
        }
    }

    /// A component tested as one batch (W28-R36): with no candidate in flight, the window's top
    /// change gets one on the stack of every change ahead of it in the window, and its pass lands
    /// them all; the window's critical path is that candidate's.
    fn admit_batch(&mut self, comp: &[usize]) {
        if comp.iter().any(|i| self.cand_of.contains_key(i)) {
            return;
        }
        let open = comp.first().and_then(|h| self.windows.get(h)).map_or(1, Speculation::open);
        let narrowed = comp.first().and_then(|h| self.limit.get(h).copied());
        let open = usize::try_from(open).unwrap_or(1);
        // A batch that doesn't fill the window waits for more changes, never past its oldest
        // change's hold (bounded by the aging bound).
        let hold = self.order.hold_s.min(self.order.aging_bound());
        let ready = comp.first().and_then(|h| self.items.get(*h)).map_or(self.now, |it| it.at.saturating_add(hold));
        if narrowed.is_none() && comp.len() < open && self.now < ready {
            if self.wakes.insert(ready) {
                self.at(ready, Ev::Wake);
            }
            return;
        }
        if let Some(h) = comp.first() {
            self.limit.remove(h);
        }
        let n = open.min(comp.len()).min(narrowed.unwrap_or(usize::MAX).max(1));
        let batch: Vec<usize> = comp.iter().take(n).copied().collect();
        let Some(&top) = batch.last() else { return };
        let c = self.new_cand(top, batch.clone());
        self.cand_of.insert(top, c);
        self.note(format!("#{top}: candidate {c}, the window's top, for {} change(s)", batch.len()));
        self.drive(c);
    }

    /// Start change `i`'s candidate and answer what it can at once, so what it queues counts
    /// against the next one weighed.
    fn start(&mut self, i: usize, what: String) {
        let c = self.new_cand(i, vec![i]);
        self.cand_of.insert(i, c);
        self.eligible.remove(&i);
        self.deferred.remove(&i);
        self.note(format!("#{i}: candidate {c}, {what}"));
        self.drive(c);
    }

    /// Decide the candidates whose jobs are all answered. A failure is the change's own only once
    /// every change ahead of it in its component has passed, as Zuul has it: one behind a change
    /// that failed or hasn't passed yet may have failed for that change, so it waits for it. An
    /// own failure ejects its change and halves its component's window, and the candidates behind
    /// re-key before anything lands; a pass opens the window by one; each component's passed
    /// prefix lands.
    fn decide(&mut self) -> bool {
        if self.order.batch {
            return self.decide_batch();
        }
        let mut passes = Vec::new();
        let mut failures = Vec::new();
        for comp in &self.comps {
            let mut clear = true;
            for &i in comp {
                let gone = self.out.get(i).is_some_and(|o| o.landed.is_some() || o.ejected);
                if gone || self.passed.contains(&i) {
                    continue;
                }
                let decided = self.cand_of.get(&i).and_then(|&c| self.cands.get(c).map(|cd| (c, cd))).filter(|(_, cd)| !cd.decided && cd.jobs.iter().all(|s| matches!(s, JobSt::Done(_) | JobSt::Skipped)));
                match decided {
                    Some((c, cd)) if cd.jobs.iter().all(|s| matches!(s, JobSt::Done(e) if e.passed())) => {
                        passes.push((i, c));
                        continue;
                    }
                    Some((c, cd)) if clear => {
                        let failed: Vec<String> = cd.jobs.iter().zip(cd.specs.iter()).filter(|(s, _)| matches!(s, JobSt::Done(e) if !e.passed())).map(|(_, sp)| sp.key.clone()).collect();
                        failures.push((i, c, failed));
                    }
                    _ => {}
                }
                clear = false;
            }
        }
        if !failures.is_empty() {
            for (i, c, failed) in failures {
                if let Some(cd) = self.cands.get_mut(c) {
                    cd.decided = true;
                }
                let w = self.window_op(i, Speculation::fail);
                self.note(format!("#{i} ejected: {} failed on its stack; its component's window halves to {w}", failed.join(", ")));
                self.eject(i);
            }
            return true;
        }
        let mut changed = false;
        for (i, c) in passes {
            if let Some(cd) = self.cands.get_mut(c) {
                cd.decided = true;
            }
            self.passed.insert(i);
            let w = self.window_op(i, Speculation::pass);
            self.note(format!("#{i} passed on its stack; its component's window opens to {w}"));
            changed = true;
        }
        for comp in self.comps.clone() {
            for i in comp {
                if self.out.get(i).is_some_and(|o| o.landed.is_some() || o.ejected) {
                    continue;
                }
                if !self.passed.contains(&i) {
                    break;
                }
                self.land(i);
                changed = true;
            }
        }
        changed
    }

    /// Decide each component's batch (W28-R36): a pass lands every change it holds and opens the
    /// window by one; a failure of one change alone ejects it; a failure of more halves the window,
    /// so the next batch is half as big, as today's bisection, but per component.
    fn decide_batch(&mut self) -> bool {
        let live: Vec<(usize, usize)> = self.cand_of.iter().map(|(&i, &c)| (i, c)).collect();
        let mut changed = false;
        for (i, c) in live {
            let Some(cd) = self.cands.get(c) else { continue };
            // A batch fails as soon as one of its jobs does: what else runs for it stops.
            let failed = cd.jobs.iter().any(|s| matches!(s, JobSt::Done(e) if !e.passed()));
            if cd.decided || !(failed || cd.jobs.iter().all(|s| matches!(s, JobSt::Done(_) | JobSt::Skipped))) {
                continue;
            }
            let ok = !failed && cd.jobs.iter().all(|s| matches!(s, JobSt::Done(e) if e.passed()));
            let batch = cd.batch.clone();
            if let Some(cd) = self.cands.get_mut(c) {
                cd.decided = true;
            }
            changed = true;
            if ok {
                self.passed.extend(batch.iter().copied());
                // The window opens by one for each change the batch lands.
                let mut w = 0;
                for _ in &batch {
                    w = self.window_op(i, Speculation::pass);
                }
                self.note(format!("#{i}'s batch of {} passed; its component's window opens to {w}", batch.len()));
            } else if batch.len() <= 1 {
                let w = self.window_op(i, Speculation::fail);
                self.note(format!("#{i} ejected: its candidate failed alone; its component's window is {w}"));
                self.eject(i);
            } else {
                // The failing jobs' classes narrow the suspects to the batch's changes of them
                // (W28-R32: a change can only break the units it affects): the next batch is the
                // changes ahead of the first suspect, or that suspect alone.
                let failed: BTreeSet<String> = self
                    .cands
                    .get(c)
                    .map(|cd| cd.jobs.iter().zip(cd.specs.iter()).filter(|(s, _)| matches!(s, JobSt::Done(e) if !e.passed())).map(|(_, sp)| sp.class.clone()).collect())
                    .unwrap_or_default();
                let suspects: Vec<usize> = batch.iter().copied().filter(|m| self.items.get(*m).is_some_and(|it| it.aff.iter().any(|a| failed.contains(a)))).collect();
                let first = batch.iter().position(|m| suspects.first() == Some(m));
                let w = self.window_op(i, Speculation::fail);
                self.cand_of.remove(&i);
                self.drop_jobs(c);
                if let [only] = suspects.as_slice() {
                    // The only change in the batch to what failed: the failing trees are the ones
                    // it has alone, so it fails alone too; out, and the rest goes on without it.
                    let only = *only;
                    self.note(format!("#{only} ejected: the only change in #{i}'s batch of {} to what failed; its component's window halves to {w}", batch.len()));
                    self.eject(only);
                    continue;
                }
                // Bisect over the suspects: the next batch ends at the middle one.
                let mid = suspects.get(suspects.len().saturating_sub(1) / 2).and_then(|m| batch.iter().position(|x| x == m));
                let cut = mid.map(|p| p + 1);
                if let (Some(p), Some(&head)) = (cut, batch.first()) {
                    self.limit.insert(head, p);
                }
                let next = first.map_or(w as usize, |p| p.max(1).min(w as usize));
                self.note(format!("#{i}'s batch of {} failed: its component's window halves to {w}, and the next batch holds {next}", batch.len()));
            }
        }
        for comp in self.comps.clone() {
            for i in comp {
                if self.out.get(i).is_some_and(|o| o.landed.is_some() || o.ejected) {
                    continue;
                }
                if !self.passed.contains(&i) {
                    break;
                }
                self.land(i);
                changed = true;
            }
        }
        changed
    }

    /// Apply `op` to the window of `i`'s component, every member's copy; its width after.
    fn window_op(&mut self, i: usize, op: fn(&mut Speculation)) -> u32 {
        let Some(mut w) = self.windows.get(&i).copied() else { return 0 };
        op(&mut w);
        self.window_max = self.window_max.max(w.max);
        let members: Vec<usize> = self.comp_of.get(&i).and_then(|k| self.comps.get(*k)).cloned().unwrap_or_else(|| vec![i]);
        for m in members {
            if let Some(slot) = self.windows.get_mut(&m) {
                *slot = w;
            }
        }
        w.open()
    }

    fn land(&mut self, i: usize) {
        if let Some(o) = self.out.get_mut(i) {
            o.landed = Some(self.now);
        }
        self.pending.retain(|&x| x != i);
        self.passed.remove(&i);
        if let Some(c) = self.cand_of.remove(&i) {
            self.drop_jobs(c);
        }
        let aff = self.items.get(i).map(|it| it.aff.clone()).unwrap_or_default();
        for class in aff {
            let t = self.trunk.entry(class).or_insert((0, 0));
            t.0 += 1;
            t.1 = t.1.wrapping_add(mix(i));
        }
        self.note(format!("#{i} landed"));
    }

    fn eject(&mut self, i: usize) {
        if let Some(o) = self.out.get_mut(i) {
            o.ejected = true;
        }
        self.pending.retain(|&x| x != i);
        self.passed.remove(&i);
        if let Some(c) = self.cand_of.remove(&i) {
            self.drop_jobs(c);
        }
    }

    // ------------------------------------------------------------------ jobs

    /// Answer what a candidate's jobs can be asked: a job whose needs all passed resolves; one with
    /// a need that failed or was skipped is skipped.
    fn drive(&mut self, c: usize) {
        let needs: Vec<Vec<usize>> = self.cands.get(c).map(|cd| cd.specs.iter().map(|s| s.needs.clone()).collect()).unwrap_or_default();
        loop {
            let mut moved = false;
            for (j, ns) in needs.iter().enumerate() {
                let Some(cd) = self.cands.get(c) else { return };
                if cd.jobs.get(j) != Some(&JobSt::Queued) {
                    continue;
                }
                let st = |n: &usize| cd.jobs.get(*n).copied();
                let blocked = ns.iter().any(|n| matches!(st(n), Some(JobSt::Skipped)) || matches!(st(n), Some(JobSt::Done(e)) if !e.passed()));
                let ready = ns.iter().all(|n| matches!(st(n), Some(JobSt::Done(e)) if e.passed()));
                if blocked {
                    self.set(c, j, JobSt::Skipped);
                    moved = true;
                } else if ready {
                    self.resolve(c, j);
                    moved = true;
                }
            }
            if !moved {
                return;
            }
        }
    }

    /// Answer candidate `c`'s job `j` on its stack's tree: under components a result the ledger
    /// has, the execution in flight, or a new execution; under serial always a new one.
    fn resolve(&mut self, c: usize, j: usize) {
        let Some((i, batch)) = self.cands.get(c).map(|cd| (cd.item, cd.batch.clone())) else { return };
        let Some((key, class, critical, src)) = self.cands.get(c).and_then(|cd| cd.specs.get(j)).map(|s| (s.key.clone(), s.class.clone(), s.critical, s.src)) else {
            self.set(c, j, JobSt::Skipped);
            return;
        };
        let serial = self.serial.is_some();
        let stack = if serial { batch } else { self.stack(i) };
        let key = format!("{key}@{}", self.tree(&class, &stack));
        if let Some(k) = self.cands.get_mut(c).and_then(|cd| cd.keys.get_mut(j)) {
            *k = Some(key.clone());
        }
        if !serial {
            if let Some(&end) = self.ledger.get(&key) {
                self.set(c, j, JobSt::Done(end));
                return;
            }
            if let Some(&x) = self.inflight.get(&key) {
                self.attach(c, j, x);
                return;
            }
        }
        let critical = !serial && critical && (self.order.batch || self.is_head(i));
        let x = self.new_exec(src.0, src.1, key.clone(), critical);
        if !serial {
            self.inflight.insert(key, x);
        }
        self.attach(c, j, x);
    }

    /// A new execution of change `i`'s job `j` at `key`, waiting for a runner. It takes the
    /// change's drawn duration, and ends as the job on this tree ended before, else as drawn.
    fn new_exec(&mut self, i: usize, j: usize, key: String, critical: bool) -> usize {
        let (runtime, drawn) = self.draw(i, j).map_or((60, End::Succeeded), |d| (d.duration.max(0), d.end));
        let outcome = self.ledger.get(&key).copied().unwrap_or(drawn);
        self.execs.push(Exec { item: i, job: j, key, queued: self.now, placed: None, started: None, ended: None, end: None, stale: false, pool: None, runtime, critical, outcome });
        self.st.push(ExecSt::Queued);
        self.serves.push(BTreeSet::new());
        let x = self.execs.len() - 1;
        self.waiting.insert(x);
        x
    }

    fn attach(&mut self, c: usize, j: usize, x: usize) {
        self.set(c, j, JobSt::Attached(x));
        if let Some(s) = self.serves.get_mut(x) {
            s.insert((c, j));
        }
        self.orphans.remove(&x);
        if self.st.get(x) == Some(&ExecSt::Running) {
            self.started(c);
        }
    }

    /// Let go of what answers candidate `c`'s job `j`; an execution left with no demand is
    /// stopped once the queue settles, unless a demand returns to it first.
    fn detach(&mut self, c: usize, j: usize) {
        let Some(JobSt::Attached(x)) = self.cands.get(c).and_then(|cd| cd.jobs.get(j)).copied() else { return };
        if let Some(s) = self.serves.get_mut(x) {
            s.remove(&(c, j));
            if s.is_empty() {
                self.orphans.insert(x);
            }
        }
    }

    fn drop_jobs(&mut self, c: usize) {
        let n = self.cands.get(c).map_or(0, |cd| cd.jobs.len());
        for j in 0..n {
            self.detach(c, j);
        }
    }

    /// The candidate's changes had work running for them.
    fn started(&mut self, c: usize) {
        let now = self.now;
        let who = self.cands.get(c).map(|cd| cd.batch.clone()).unwrap_or_default();
        for i in who {
            if let Some(o) = self.out.get_mut(i)
                && o.started.is_none()
            {
                o.started = Some(now);
            }
        }
    }

    // ------------------------------------------------------------------ runners

    /// The pooled queue (W28-R35): under components, every waiting execution ranked by the order,
    /// one past the aging bound keeping what it waits for from anything ranked below it; under
    /// serial, in queue order. Each goes to the least-loaded pool with room that carries its labels.
    fn dispatch(&mut self) {
        let mut queue: Vec<usize> = self.waiting.iter().copied().collect();
        let components = self.serial.is_none();
        if components {
            let ws: Vec<Waiting> = queue
                .iter()
                .map(|&x| {
                    let e = self.execs.get(x);
                    let service_s = e.and_then(|e| self.items.get(e.item).and_then(|it| it.check.get(e.job))).map_or(60, |s| s.typical);
                    Waiting { critical: e.is_some_and(|e| e.critical), service_s, waited_s: self.now - e.map_or(self.now, |e| e.queued) }
                })
                .collect();
            queue = self.order.rank(&ws).into_iter().filter_map(|k| queue.get(k).copied()).collect();
        }
        let bound = self.order.aging_bound();
        let mut held: BTreeSet<usize> = BTreeSet::new();
        for x in queue {
            let pools = self.execs.get(x).and_then(|e| self.trace_job(e.item, e.job)).map(|j| self.carriers(j)).unwrap_or_default();
            let waited = self.now - self.execs.get(x).map_or(self.now, |e| e.queued);
            let open: Vec<usize> = pools.iter().copied().filter(|p| !held.contains(p) && self.room(*p, x)).collect();
            let best = open.into_iter().map(|p| (p, self.util(p))).fold(None, |b: Option<(usize, f64)>, (p, u)| if b.is_none_or(|(_, bu)| u < bu) { Some((p, u)) } else { b });
            match best {
                Some((p, _)) => self.place(x, p),
                None if components && waited >= bound => held.extend(pools),
                None => {}
            }
        }
    }

    /// The pools that carry every label `job` asks for.
    fn carriers(&self, job: &trace::Job) -> Vec<usize> {
        self.trace.pools.iter().enumerate().filter(|(_, p)| super::carries(p, job)).map(|(i, _)| i).collect()
    }

    /// The share of pool `p`'s capacity committed (W28-R37): what holds it, and what waits for it
    /// (an execution several pools could take counted a share at each).
    fn committed(&self, p: usize) -> f64 {
        let (Some(spec), Some(ps)) = (self.trace.pools.get(p), self.pools.get(p)) else { return 0.0 };
        let (mut busy, mut cpu, mut mem, mut pods) = (f64::from(ps.busy), ps.cpu as f64, ps.mem as f64, f64::from(ps.pods));
        for &x in &self.waiting {
            let Some(job) = self.execs.get(x).and_then(|e| self.trace_job(e.item, e.job)) else { continue };
            let carriers = self.carriers(job);
            if !carriers.contains(&p) {
                continue;
            }
            let share = 1.0 / carriers.len() as f64;
            let (c, m) = self.req(x);
            busy += share;
            cpu += c as f64 * share;
            mem += m as f64 * share;
            pods += share;
        }
        match spec.slots {
            Some(slots) => busy / f64::from(slots.max(1)),
            None => {
                let cpu = spec.cpu_milli.map_or(0.0, |c| cpu / c.max(1) as f64);
                let mem = spec.mem_mib.map_or(0.0, |m| mem / m.max(1) as f64);
                let pods = spec.pods.map_or(0.0, |n| pods / f64::from(n.max(1)));
                cpu.max(mem).max(pods)
            }
        }
    }

    /// The bottleneck change `i`'s check needs (W28-R37): of each of its jobs' pools the least
    /// committed, and of those the most.
    fn bottleneck(&self, i: usize) -> f64 {
        let Some(run) = self.items.get(i).and_then(|it| self.trace.runs.get(it.run)) else { return 0.0 };
        run.jobs.iter().map(|j| self.carriers(j).into_iter().map(|p| self.committed(p)).fold(f64::INFINITY, f64::min)).filter(|u| u.is_finite()).fold(0.0, f64::max)
    }

    fn req(&self, x: usize) -> (u64, u64) {
        self.execs.get(x).and_then(|e| self.trace_job(e.item, e.job)).and_then(|j| j.req).unwrap_or((1000, 2048))
    }

    /// Whether pool `p` has room for execution `x`: a free slot on a machine, or its requests
    /// within the k8s quota.
    fn room(&self, p: usize, x: usize) -> bool {
        let (Some(spec), Some(ps)) = (self.trace.pools.get(p), self.pools.get(p)) else { return false };
        match spec.slots {
            Some(slots) => ps.busy < slots,
            None => {
                let (c, m) = self.req(x);
                ps.cpu + c <= spec.cpu_milli.unwrap_or(u64::MAX) && ps.mem + m <= spec.mem_mib.unwrap_or(u64::MAX) && ps.pods < spec.pods.unwrap_or(u32::MAX)
            }
        }
    }

    /// A pool's share of its capacity in use: busy slots on a machine; the most used of cpu,
    /// memory and pods on the k8s quota.
    fn util(&self, p: usize) -> f64 {
        let (Some(spec), Some(ps)) = (self.trace.pools.get(p), self.pools.get(p)) else { return 0.0 };
        match spec.slots {
            Some(slots) => f64::from(ps.busy) / f64::from(slots.max(1)),
            None => {
                let cpu = spec.cpu_milli.map_or(0.0, |c| ps.cpu as f64 / c.max(1) as f64);
                let mem = spec.mem_mib.map_or(0.0, |m| ps.mem as f64 / m.max(1) as f64);
                let pods = spec.pods.map_or(0.0, |n| f64::from(ps.pods) / f64::from(n.max(1)));
                cpu.max(mem).max(pods)
            }
        }
    }

    fn place(&mut self, x: usize, p: usize) {
        let quota = self.trace.pools.get(p).is_some_and(|s| s.slots.is_none());
        let (c, m) = self.req(x);
        if let Some(ps) = self.pools.get_mut(p) {
            if quota {
                ps.cpu += c;
                ps.mem += m;
                ps.pods += 1;
            } else {
                ps.busy += 1;
            }
        }
        let now = self.now;
        let delay = if quota { self.execs.get(x).and_then(|e| self.draw(e.item, e.job)).map_or(0, |d| d.delay) } else { 0 };
        if let Some(e) = self.execs.get_mut(x) {
            e.pool = Some(p);
            e.placed = Some(now);
        }
        self.waiting.remove(&x);
        if delay > 0 {
            if let Some(s) = self.st.get_mut(x) {
                *s = ExecSt::Launching;
            }
            self.at(now + delay, Ev::Launched(x));
        } else {
            self.begin(x);
        }
    }

    fn launched(&mut self, x: usize) {
        if self.st.get(x) == Some(&ExecSt::Launching) {
            self.begin(x);
        }
    }

    fn begin(&mut self, x: usize) {
        let now = self.now;
        let Some(e) = self.execs.get_mut(x) else { return };
        e.started = Some(now);
        let rt = e.runtime;
        if let Some(s) = self.st.get_mut(x) {
            *s = ExecSt::Running;
        }
        self.at(now + rt, Ev::Finish(x));
        let served: Vec<usize> = self.serves.get(x).map(|s| s.iter().map(|(c, _)| *c).collect()).unwrap_or_default();
        for c in served {
            self.started(c);
        }
    }

    /// Free what an execution holds of its pool.
    fn release(&mut self, x: usize) {
        let Some(p) = self.execs.get(x).and_then(|e| e.pool) else { return };
        let quota = self.trace.pools.get(p).is_some_and(|s| s.slots.is_none());
        let (c, m) = self.req(x);
        let Some(ps) = self.pools.get_mut(p) else { return };
        if quota {
            ps.cpu = ps.cpu.saturating_sub(c);
            ps.mem = ps.mem.saturating_sub(m);
            ps.pods = ps.pods.saturating_sub(1);
        } else {
            ps.busy = ps.busy.saturating_sub(1);
        }
    }

    fn finish(&mut self, x: usize) {
        if self.st.get(x) != Some(&ExecSt::Running) {
            return;
        }
        self.release(x);
        let now = self.now;
        let Some(e) = self.execs.get_mut(x) else { return };
        e.ended = Some(now);
        e.end = Some(e.outcome);
        let (key, end) = (e.key.clone(), e.outcome);
        if let Some(s) = self.st.get_mut(x) {
            *s = ExecSt::Over;
        }
        if self.inflight.get(&key) == Some(&x) {
            self.inflight.remove(&key);
        }
        self.ledger.insert(key, end);
        let served = self.serves.get_mut(x).map(std::mem::take).unwrap_or_default();
        for (c, j) in served {
            if self.cands.get(c).and_then(|cd| cd.jobs.get(j)) == Some(&JobSt::Attached(x)) {
                self.set(c, j, JobSt::Done(end));
            }
        }
    }

    /// Stop an execution no candidate wants any more.
    fn cancel(&mut self, x: usize) {
        match self.st.get(x).copied() {
            Some(ExecSt::Queued) => {
                self.waiting.remove(&x);
            }
            Some(ExecSt::Launching | ExecSt::Running) => self.release(x),
            _ => return,
        }
        let now = self.now;
        if let Some(e) = self.execs.get_mut(x) {
            e.ended = Some(now);
            e.stale = true;
        }
        if let Some(s) = self.st.get_mut(x) {
            *s = ExecSt::Over;
        }
        let key = self.execs.get(x).map(|e| e.key.clone());
        if let Some(k) = key
            && self.inflight.get(&k) == Some(&x)
        {
            self.inflight.remove(&k);
        }
    }
}

/// A duration as the report gives it.
fn dur(s: f64) -> String {
    if s < 3600.0 { format!("{:.0} min", s / 60.0) } else { format!("{:.1} h", s / 3600.0) }
}

fn mean(xs: impl Iterator<Item = f64>) -> f64 {
    let (mut sum, mut n) = (0.0, 0usize);
    for x in xs {
        sum += x;
        n += 1;
    }
    if n == 0 { 0.0 } else { sum / n as f64 }
}

fn change(from: f64, to: f64) -> String {
    if from == 0.0 {
        return "–".to_owned();
    }
    match ((to - from) / from * 100.0).round() as i64 {
        0 => "0%".to_owned(),
        p => format!("{p:+}%"),
    }
}

/// The land queue's report (W28-AC33), as ci/sim/README.md keeps it: [`LandPolicy::pair`] under
/// each seed, then the means over the seeds side by side.
pub fn report(trace: &Trace, seeds: &[u64]) -> String {
    let pair = LandPolicy::pair();
    let outs: Vec<(String, Vec<LandOut>)> = pair.iter().map(|p| (p.name.clone(), seeds.iter().map(|&s| replay(trace, p, s)).collect())).collect();
    let changes = outs.first().and_then(|(_, o)| o.first()).map_or(0, |o| o.items.len());
    let span = match (seeds.first(), seeds.last()) {
        (Some(a), Some(b)) if a != b => format!("{a}-{b}"),
        (Some(a), _) => a.to_string(),
        _ => String::new(),
    };
    let mut s = String::new();
    let _ = writeln!(s, "Seeds {span}; the week's {changes} ci pushes to main are the changes that want to land.");
    let _ = writeln!(s);
    let _ = writeln!(s, "| Policy | Seed | Landed | Ejected | Mean enqueue to landed | Runner-h per landed change | Longest wait for a runner | Widest window |");
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|");
    for (name, os) in &outs {
        for o in os {
            let (landed, ejected) = o.fates();
            let window = if o.window_max == 0 { "–".to_owned() } else { format!("{} (cap {})", o.window_max, o.window_cap) };
            let _ = writeln!(s, "| {name} | {} | {landed} | {ejected} | {} | {:.3} | {} | {window} |", o.seed, dur(o.mean_to_land()), o.runner_hours_per_landed(), dur(o.max_wait() as f64));
        }
    }
    let _ = writeln!(s);
    let names: Vec<&str> = outs.iter().map(|(n, _)| n.as_str()).collect();
    let _ = writeln!(s, "| Mean over seeds {span} | {} | Change |", names.join(" | "));
    let _ = writeln!(s, "|---|{}---|", "---|".repeat(names.len()));
    let rows: [(&str, fn(&LandOut) -> f64, fn(f64) -> String); 4] = [
        ("Mean enqueue to landed", LandOut::mean_to_land, dur),
        ("Runner-h per landed change", LandOut::runner_hours_per_landed, |x| format!("{x:.3}")),
        ("Changes landed", |o| o.fates().0 as f64, |x| format!("{x:.1}")),
        ("Longest wait for a runner", |o| o.max_wait() as f64, dur),
    ];
    for (label, f, show) in rows {
        let means: Vec<f64> = outs.iter().map(|(_, os)| mean(os.iter().map(f))).collect();
        let shown: Vec<String> = means.iter().map(|m| show(*m)).collect();
        let delta = match (means.first(), means.last()) {
            (Some(a), Some(b)) if means.len() > 1 => change(*a, *b),
            _ => "–".to_owned(),
        };
        let _ = writeln!(s, "| {label} | {} | {delta} |", shown.join(" | "));
    }
    s
}

/// The sweeps the order's defaults come from (ci/sim/README.md): the window's cap and the
/// admission cap, each varied with the others at their defaults; means over `seeds`, today's
/// queue first.
pub fn sweeps(trace: &Trace, seeds: &[u64]) -> String {
    let mut policies = vec![LandPolicy::serial(1)];
    for cap in [1, 2, 4, 8, 12] {
        policies.push(LandPolicy { name: format!("components, window cap {cap}"), mode: Mode::Components(Order { window: admit::Window { initial: cap, floor: 1, cap }, ..Order::default() }) });
    }
    for (cap, aging, hold) in [(8, 1.0, 0), (8, 1.0, 3600), (4, 0.75, 4800), (8, 0.75, 4800)] {
        policies.push(LandPolicy { name: format!("components, batches, window cap {cap}, aging {aging}, hold {} min", hold / 60), mode: Mode::Components(Order { aging_per_minute: aging, window: admit::Window { initial: cap, floor: 1, cap }, batch: true, hold_s: hold, ..Order::default() }) });
    }
    for util in [0.6, 0.8, 1.0] {
        policies.push(LandPolicy { name: format!("components, admission cap {util}"), mode: Mode::Components(Order { util_cap: util, ..Order::default() }) });
    }
    let mut s = String::new();
    let _ = writeln!(s, "| Policy | Mean enqueue to landed | Runner-h per landed change | Changes landed | Longest wait for a runner | Runner-h stopped unused | Runner-h of failed jobs | Runner-h |");
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|");
    for p in &policies {
        let outs: Vec<LandOut> = seeds.iter().map(|&seed| replay(trace, p, seed)).collect();
        let stopped = mean(outs.iter().map(|o| o.execs.iter().filter(|e| e.stale).map(Exec::used).sum::<i64>() as f64 / 3600.0));
        let longest = outs.iter().map(LandOut::max_wait).max().unwrap_or(0);
        let failed = mean(outs.iter().map(|o| o.execs.iter().filter(|e| e.end.is_some_and(|x| !x.passed())).map(Exec::used).sum::<i64>() as f64 / 3600.0));
        let total = mean(outs.iter().map(|o| o.used() as f64 / 3600.0));
        let _ = writeln!(
            s,
            "| {} | {} | {:.3} | {:.1} | {} | {stopped:.1} | {failed:.1} | {total:.1} |",
            p.name,
            dur(mean(outs.iter().map(LandOut::mean_to_land))),
            mean(outs.iter().map(LandOut::runner_hours_per_landed)),
            mean(outs.iter().map(|o| o.fates().0 as f64)),
            dur(longest as f64)
        );
    }
    s
}
