//! Demands and adopt-on-supersede (docs/ci.md W28-R40 to R44), the pure part: who still wants
//! each piece of work, which legs share one execution of the same work, and what becomes of work
//! no one wants any more. Like [`super::groups`], it's a table the scheduler owns, with no clock,
//! disk or randomness: times come in as arguments, and every map is ordered, so the same inputs
//! give the same decisions.
//!
//! - Each run is a [`Demand`]: a lane, a commit, an intent, an actor and a trust domain. A newer
//!   push on its lane *withdraws* an older demand (`supersede: { action: adopt }`); it cancels
//!   nothing itself (R40).
//! - A leg adopts only executions its [`Domain`] may read: its own domain's, and trusted ones for
//!   untrusted and person scopes; protected adopts nothing (R41).
//! - The work in flight is keyed by key and domain: a leg with a key adopts the execution of that
//!   key in flight, or executes it, never twice at once within a domain (R42). The ledger's hits
//!   (W24-R06; W08-R10's reuse today) are the dispatcher's, before a leg is queued: this table
//!   never reads storage.
//! - Work no live demand wants is dropped before it starts; once running, it runs on until live
//!   work contends for its capacity, then finishes if it's within `finish_within` of its end and
//!   seeds trusted entries, and is cancelled otherwise ([`Fate`], R43). The table never touches
//!   the ledger: what a cancelled execution finished stays there.
//! - When an execution ends, its adopters take its result over for a success or a failure of class
//!   `failure`; on any other end, the first live adopter executes in its place ([`Next`], R44).

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use super::retry::FailureClass;

/// `finish_within`'s default, in seconds: work no one wants finishes, when live work contends for
/// its capacity, if it has at most this long left. W25 step 1's sweep (ci/sim/README.md): under 1%
/// of runner time wasted, within about a point of the most verdicts.
pub const FINISH_WITHIN: i64 = 300;

/// What a run's verdict is wanted for (W28-R40).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Intent {
    /// The land queue's run, or a merge group's: a land waits on it.
    Gate,
    /// Someone waits for it: a dispatch, an agent's check.
    Waiting,
    /// A push to the default branch.
    Trunk,
    /// Work in progress: other branches' pushes, diffs.
    #[default]
    Wip,
}

/// The trust domain work runs and is kept in (W24-R21).
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Domain {
    /// Default-branch pushes and the land queue.
    Trusted,
    /// Release and deploy legs.
    Protected,
    /// Forks and outside contributors, and by default diffs and agents' events.
    #[default]
    Untrusted,
    /// An agent's own builds, in the scope of the person it acts for.
    Person(String),
}

impl Domain {
    /// Whether a leg in this domain may adopt an execution in `writer` (W28-R41): its own domain's,
    /// and for untrusted and person scopes trusted ones too, as reads go down only (W24-R21).
    /// Protected adopts nothing: it takes only results two trusted runners produced identically
    /// (W24 Decision 4), and an execution in flight is one runner's.
    pub fn may_adopt(&self, writer: &Domain) -> bool {
        match (self, writer) {
            (Domain::Protected, _) => false,
            (Domain::Untrusted | Domain::Person(_), Domain::Trusted) => true,
            (reader, writer) => reader == writer,
        }
    }
}

impl fmt::Display for Domain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Domain::Trusted => f.write_str("trusted"),
            Domain::Protected => f.write_str("protected"),
            Domain::Untrusted => f.write_str("untrusted"),
            Domain::Person(id) => write!(f, "person:{id}"),
        }
    }
}

/// A run's demand (W28-R40): someone wants verdicts on a commit of a lane.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Demand {
    pub run: String,
    /// What a newer push supersedes it on: the repository, the workflow, and the branch, stack or
    /// pull request (W05-R08's `when`).
    pub lane: String,
    pub commit: String,
    pub intent: Intent,
    /// Who pushed it: an agent, or the person.
    pub actor: Option<String>,
    pub domain: Domain,
    /// When its run was created (unix ms, or any order the caller keeps): a demand withdraws only
    /// older ones, so a run approved late (W18-R06) doesn't withdraw a newer one.
    pub at: i64,
}

/// The work a leg asks for (W28-R42).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Work {
    /// Equal keys, equal work: W08-R10's input hash today, W24's strong key once it's built. A leg
    /// without one adopts nothing and is never adopted.
    pub key: Option<String>,
    /// The domain it runs in, and its results are kept in.
    pub domain: Domain,
    /// Its warm workspace's lane (W24-R11: the repository, the job, the matrix values that matter,
    /// the toolchain and the profile), which a cancelled execution's workspace goes back to.
    pub lane: String,
    /// It reads observed path sets (W24-R04): an adopter checks them at its own commit before it
    /// takes the result over.
    pub observed: bool,
}

impl Work {
    /// Whether its results become trusted entries (W28-R43): a trusted execution with a key.
    pub fn seeds_trusted(&self) -> bool {
        self.key.is_some() && self.domain == Domain::Trusted
    }
}

/// What a leg's [`Demands::ask`] gets (W28-R42).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "ask", rename_all = "kebab-case")]
pub enum Ask {
    /// It adopts `exec`, the leg of `run` that executes the same work: it holds no runner, and
    /// finishes reused from it.
    Adopt { exec: String, run: String },
    /// It executes the work.
    Execute,
    /// Its run's demand was withdrawn and nothing in flight has its work: no one wants it run.
    Drop,
}

impl Ask {
    /// Why, in words, for the leg's routing and its run's page (W28-R47).
    pub fn why(&self) -> String {
        match self {
            Ask::Adopt { exec, run } => format!("adopts {exec} of run {run}: the same work, in flight in a domain it may read"),
            Ask::Execute => "executes: nothing in flight that it may adopt has its key".to_owned(),
            Ask::Drop => "dropped: its run's demand was withdrawn, and nothing in flight has its work".to_owned(),
        }
    }
}

/// What a leg its runner still runs after a restart is (W28-R42).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "running", rename_all = "kebab-case")]
pub enum Running {
    /// The execution of its work.
    Executes,
    /// The execution of its work, which `queued` (asked for before this one was announced) adopts
    /// instead, with its own adopters: `queued` stops waiting for a runner.
    Took { queued: String },
    /// Another execution of the same work runs already: two at once, which can't be undone. This
    /// one runs on alone, shared with no one.
    Alone { exec: String },
}

/// How an execution ended, as far as its adopters go (W28-R44).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum End {
    Success,
    Failed(FailureClass),
    Cancelled,
}

impl End {
    /// Whether adopters take it over: a success, or a failure of class `failure`, which is the
    /// work's own. Never runner-lost, infra, image-pull, oom, disk-full or timeout, which are the
    /// runner's or the moment's, nor a cancel.
    pub fn shared(self) -> bool {
        matches!(self, End::Success | End::Failed(FailureClass::Failure))
    }
}

/// What becomes of an execution (W28-R43), decided after a withdrawal, when a waiter leaves, or
/// when live work contends for its capacity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "fate", rename_all = "kebab-case")]
pub enum Fate {
    /// Live demands still want it (`by`, their runs): it goes on, at the highest priority they
    /// give.
    Kept { priority: u8, by: Vec<String> },
    /// No live demand wants it and it hasn't started: it's dropped. `by`: the runs that withdrew
    /// the demands it answered.
    Drop { by: Vec<String> },
    /// No live demand wants it, but nothing contends for its capacity: it runs on.
    Run { by: Vec<String> },
    /// Contended, but it has about `left` s left, within `finish_within`, and it seeds trusted
    /// entries: it finishes.
    Finish { by: Vec<String>, left: i64, finish_within: i64 },
    /// Contended: it's cancelled for live work, its warm workspace going back to `lane`. `left` is
    /// how long it was expected to take still, when known; `seeds`, whether it seeds trusted
    /// entries.
    Cancel { by: Vec<String>, left: Option<i64>, finish_within: i64, seeds: bool, lane: String },
}

impl Fate {
    /// Why, in words, for the leg's routing and its run's page (W28-R47).
    pub fn why(&self) -> String {
        match self {
            Fate::Kept { priority, by } => format!("kept: {} still {} it, so it goes on at priority {priority}", runs(by), if by.len() == 1 { "wants" } else { "want" }),
            Fate::Drop { by } => format!("dropped before it started: no live demand wants it ({})", withdrawn(by)),
            Fate::Run { by } => format!("runs on: no live demand wants it ({}), but nothing waits for its capacity", withdrawn(by)),
            Fate::Finish { by, left, finish_within } => {
                format!("finishes: no live demand wants it ({}), but it has about {left} s left, within finish_within ({finish_within} s), and it seeds trusted entries", withdrawn(by))
            }
            Fate::Cancel { by, left, finish_within, seeds, lane } => {
                let because = match (left, seeds) {
                    (_, false) => "it seeds no trusted entries".to_owned(),
                    (Some(left), true) => format!("it has about {left} s left, more than finish_within ({finish_within} s)"),
                    (None, true) => "how long it has left isn't known".to_owned(),
                };
                format!("cancelled for live work: no live demand wants it ({}), and {because}; its warm workspace goes back to lane {lane}", withdrawn(by))
            }
        }
    }
}

/// What an adopter does next, once its execution ended or its check came back (W28-R44).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "next", rename_all = "kebab-case")]
pub enum Next {
    /// It takes `from`'s result over: it finishes reused from it, naming it.
    Reuse { from: String },
    /// `from` read observed path sets: it checks them at its own commit first (W24-R04), and
    /// [`Demands::checked`] says what it does then.
    Check { from: String },
    /// It executes, in place of an execution whose end it may not take over.
    Execute,
    /// It adopts another execution of the same work.
    Adopt { exec: String, run: String },
    /// Its run's demand was withdrawn and nothing it may take is left: it's dropped.
    Drop,
}

impl Next {
    /// Why, in words, for the leg's routing and its run's page (W28-R47).
    pub fn why(&self) -> String {
        match self {
            Next::Reuse { from } => format!("reused from {from}, the execution it adopted"),
            Next::Check { from } => format!("checks what {from} read against its own commit first (W24-R04)"),
            Next::Execute => "executes: the execution it adopted ended in a way it may not take over".to_owned(),
            Next::Adopt { exec, run } => format!("adopts {exec} of run {run}: the same work, in flight in a domain it may read"),
            Next::Drop => "dropped: its run's demand was withdrawn, and nothing it may take is left".to_owned(),
        }
    }
}

fn runs(v: &[String]) -> String {
    match v {
        [] => "no run".to_owned(),
        [one] => format!("run {one}"),
        more => format!("runs {}", more.join(", ")),
    }
}

fn withdrawn(by: &[String]) -> String {
    if by.is_empty() { "its demands are gone".to_owned() } else { format!("withdrawn by {}", runs(by)) }
}

#[derive(Debug, Clone)]
struct DemandSt {
    demand: Demand,
    /// The run that withdrew it.
    withdrawn: Option<String>,
}

/// A leg waiting on an execution, or checking one that ended.
#[derive(Debug, Clone)]
struct Waiter {
    leg: String,
    run: String,
    priority: u8,
    work: Work,
}

#[derive(Debug, Clone)]
struct Exec {
    run: String,
    work: Work,
    priority: u8,
    /// When its attempt started (unix ms), while one runs.
    started: Option<i64>,
    /// Its adopters, in the order they adopted.
    waiters: Vec<Waiter>,
    /// Others may adopt it: it's the in-flight table's for its key and domain.
    shared: bool,
}

/// The demands and the work in flight (W28-R40 to R44).
#[derive(Debug, Clone, Default)]
pub struct Demands {
    demands: BTreeMap<String, DemandSt>,
    /// Each lane's runs, oldest first.
    lanes: BTreeMap<String, Vec<String>>,
    /// Executions, by their leg.
    execs: BTreeMap<String, Exec>,
    /// The execution others may adopt, by key and domain.
    inflight: BTreeMap<(String, Domain), String>,
    /// The execution each adopter waits on.
    adopted: BTreeMap<String, String>,
    /// Adopters checking an ended execution's observed reads (W24-R04), and that execution.
    checking: BTreeMap<String, (Waiter, String)>,
}

impl Demands {
    /// A run's demand arrives (W28-R40). With `withdraw` (`supersede: { action: adopt }`, its
    /// branch not in `keep`), it withdraws every older live demand on its lane, which it returns,
    /// oldest first. A run's demand arriving again (a rerun) is live again, and withdraws only as
    /// `withdraw` says: a rerun's withdraws nothing.
    pub fn arrive(&mut self, demand: Demand, withdraw: bool) -> Vec<String> {
        let (run, lane, at) = (demand.run.clone(), demand.lane.clone(), demand.at);
        if let Some(old) = self.demands.get(&run)
            && old.demand.lane != lane
            && let Some(v) = self.lanes.get_mut(&old.demand.lane)
        {
            v.retain(|r| *r != run);
        }
        self.demands.insert(run.clone(), DemandSt { demand, withdrawn: None });
        let demands = &self.demands;
        let v = self.lanes.entry(lane).or_default();
        v.retain(|r| *r != run);
        // After the runs created before it or at once with it.
        let at_place = v.iter().position(|r| demands.get(r).is_some_and(|d| d.demand.at > at)).unwrap_or(v.len());
        v.insert(at_place, run.clone());
        if !withdraw {
            return Vec::new();
        }
        let older: Vec<String> = v.iter().take(at_place).filter(|r| demands.get(*r).is_some_and(|d| d.withdrawn.is_none())).cloned().collect();
        for o in &older {
            if let Some(d) = self.demands.get_mut(o) {
                d.withdrawn = Some(run.clone());
            }
        }
        older
    }

    /// A run ended: its demand goes.
    pub fn leave(&mut self, run: &str) {
        if let Some(d) = self.demands.remove(run)
            && let Some(v) = self.lanes.get_mut(&d.demand.lane)
        {
            v.retain(|r| r != run);
            if v.is_empty() {
                self.lanes.remove(&d.demand.lane);
            }
        }
    }

    /// A run's demand, while it has one.
    pub fn demand(&self, run: &str) -> Option<&Demand> {
        self.demands.get(run).map(|d| &d.demand)
    }

    /// The run that withdrew `run`'s demand.
    pub fn withdrawn_by(&self, run: &str) -> Option<&str> {
        self.demands.get(run).and_then(|d| d.withdrawn.as_deref())
    }

    /// Whether `run`'s demand is live: not withdrawn. A run without a demand (its workflow doesn't
    /// adopt) wants its own work.
    pub fn live(&self, run: &str) -> bool {
        self.demands.get(run).is_none_or(|d| d.withdrawn.is_none())
    }

    /// A leg with work to do asks for it (W28-R42): it adopts the execution of its key in flight
    /// that its domain may adopt (its own domain's first, then a trusted one), or executes it, at
    /// `priority` (W05-R09). A leg of a withdrawn run only takes what's in flight: with nothing
    /// there, it's dropped. Asked again, a leg gets what it got.
    pub fn ask(&mut self, leg: &str, run: &str, work: &Work, priority: u8) -> Ask {
        if self.execs.contains_key(leg) {
            return Ask::Execute;
        }
        if let Some(exec) = self.adopted.get(leg) {
            return Ask::Adopt { exec: exec.clone(), run: self.execs.get(exec).map(|x| x.run.clone()).unwrap_or_default() };
        }
        self.checking.remove(leg);
        if let Some(exec) = self.adoptable(work) {
            let of = self.attach(&exec, Waiter { leg: leg.to_owned(), run: run.to_owned(), priority, work: work.clone() });
            return Ask::Adopt { exec, run: of };
        }
        if !self.live(run) {
            return Ask::Drop;
        }
        self.register(leg, run, work, priority, None);
        Ask::Execute
    }

    /// After a restart, a leg its runner still runs (W28-R42, W05-R25). A queued execution of the
    /// same work, asked for before this one was announced, adopts it instead, with its adopters.
    pub fn running(&mut self, leg: &str, run: &str, work: &Work, priority: u8, since: i64) -> Running {
        if let Some(x) = self.execs.get_mut(leg) {
            x.started = Some(since);
            return Running::Executes;
        }
        self.forget(leg);
        let slot = work.key.as_ref().filter(|_| work.domain.may_adopt(&work.domain)).map(|k| (k.clone(), work.domain.clone()));
        let Some((slot, other)) = slot.and_then(|s| self.inflight.get(&s).cloned().map(|o| (s, o))) else {
            self.register(leg, run, work, priority, Some(since));
            return Running::Executes;
        };
        if self.execs.get(&other).is_some_and(|x| x.started.is_some()) {
            self.execs.insert(leg.to_owned(), Exec { run: run.to_owned(), work: work.clone(), priority, started: Some(since), waiters: Vec::new(), shared: false });
            return Running::Alone { exec: other };
        }
        let queued = self.execs.remove(&other);
        self.inflight.remove(&slot);
        self.register(leg, run, work, priority, Some(since));
        let Some(q) = queued else { return Running::Executes };
        let me = Waiter { leg: other.clone(), run: q.run, priority: q.priority, work: q.work };
        for w in std::iter::once(me).chain(q.waiters) {
            self.attach(leg, w);
        }
        Running::Took { queued: other }
    }

    /// An execution's attempt started at `at` (unix ms).
    pub fn started(&mut self, exec: &str, at: i64) {
        if let Some(x) = self.execs.get_mut(exec) {
            x.started = Some(at);
        }
    }

    /// An execution's attempt ended, and it waits again (for a retry, or a runner).
    pub fn stopped(&mut self, exec: &str) {
        if let Some(x) = self.execs.get_mut(exec) {
            x.started = None;
        }
    }

    /// Whether `leg` executes work in this table.
    pub fn is_execution(&self, leg: &str) -> bool {
        self.execs.contains_key(leg)
    }

    /// The execution an adopter waits on.
    pub fn adopts(&self, leg: &str) -> Option<&str> {
        self.adopted.get(leg).map(String::as_str)
    }

    /// An execution's adopters, in the order they adopted.
    pub fn adopters(&self, exec: &str) -> Vec<&str> {
        self.execs.get(exec).map(|x| x.waiters.iter().map(|w| w.leg.as_str()).collect()).unwrap_or_default()
    }

    /// The runs whose demands an execution answers: its own leg's run, then its adopters'.
    fn answers(x: &Exec) -> impl Iterator<Item = (&String, u8)> {
        std::iter::once((&x.run, x.priority)).chain(x.waiters.iter().map(|w| (&w.run, w.priority)))
    }

    /// An execution's priority (W28-R42): the highest among the live demands waiting on it; `None`
    /// when no live demand wants it.
    pub fn priority(&self, exec: &str) -> Option<u8> {
        let x = self.execs.get(exec)?;
        Self::answers(x).filter(|(r, _)| self.live(r)).map(|(_, p)| p).max()
    }

    /// Whether a live demand wants an execution.
    pub fn wanted(&self, exec: &str) -> bool {
        self.priority(exec).is_some()
    }

    /// The executions answering any of `runs`' demands: those to decide again after they're
    /// withdrawn.
    pub fn affected(&self, runs: &[String]) -> Vec<String> {
        self.execs.iter().filter(|(_, x)| Self::answers(x).any(|(r, _)| runs.contains(r))).map(|(k, _)| k.clone()).collect()
    }

    /// The running executions no live demand wants: those live work may contend with.
    pub fn unwanted_running(&self) -> Vec<String> {
        self.execs.iter().filter(|(k, x)| x.started.is_some() && !self.wanted(k)).map(|(k, _)| k.clone()).collect()
    }

    /// What becomes of an execution (W28-R43). `left`: the seconds it's expected to take still
    /// (its cost less its attempt's time so far), when known; `contended`: live work waits for its
    /// capacity.
    pub fn fate(&self, exec: &str, left: Option<i64>, contended: bool, finish_within: i64) -> Option<Fate> {
        let x = self.execs.get(exec)?;
        if let Some(priority) = self.priority(exec) {
            let by: BTreeSet<String> = Self::answers(x).filter(|(r, _)| self.live(r)).map(|(r, _)| r.clone()).collect();
            return Some(Fate::Kept { priority, by: by.into_iter().collect() });
        }
        let by: BTreeSet<String> = Self::answers(x).filter_map(|(r, _)| self.withdrawn_by(r)).map(str::to_owned).collect();
        let by: Vec<String> = by.into_iter().collect();
        if x.started.is_none() {
            return Some(Fate::Drop { by });
        }
        if !contended {
            return Some(Fate::Run { by });
        }
        let seeds = x.work.seeds_trusted();
        Some(match left {
            Some(left) if seeds && left <= finish_within => Fate::Finish { by, left, finish_within },
            _ => Fate::Cancel { by, left, finish_within, seeds, lane: x.work.lane.clone() },
        })
    }

    /// An execution ended (W28-R44): what each of its adopters does next. A success or a failure
    /// of class `failure` is theirs too (checked first when the work read observed path sets); on
    /// any other end, the live adopters ask again in the order they adopted, the first executing
    /// and the others adopting it where their domain may, then the withdrawn ones take what's in
    /// flight or are dropped.
    pub fn ended(&mut self, exec: &str, end: End) -> Vec<(String, Next)> {
        let Some(x) = self.execs.remove(exec) else { return Vec::new() };
        self.unregister(exec, &x);
        let mut out = Vec::new();
        if end.shared() {
            for w in x.waiters {
                self.adopted.remove(&w.leg);
                let from = exec.to_owned();
                if x.work.observed {
                    out.push((w.leg.clone(), Next::Check { from: from.clone() }));
                    self.checking.insert(w.leg.clone(), (w, from));
                } else {
                    out.push((w.leg, Next::Reuse { from }));
                }
            }
            return out;
        }
        let (live, withdrawn): (Vec<Waiter>, Vec<Waiter>) = x.waiters.into_iter().partition(|w| self.live(&w.run));
        for w in live.into_iter().chain(withdrawn) {
            self.adopted.remove(&w.leg);
            let next = self.again(&w);
            out.push((w.leg, next));
        }
        out
    }

    /// W24-R04's check of an ended execution's observed reads at an adopter's own commit came
    /// back: it takes the result over when they match, and otherwise runs itself (or adopts
    /// another execution of its work; a withdrawn one with nothing to take is dropped).
    pub fn checked(&mut self, leg: &str, matched: bool) -> Option<Next> {
        let (w, from) = self.checking.remove(leg)?;
        Some(if matched { Next::Reuse { from } } else { self.again(&w) })
    }

    /// An adopter went away (its run cancelled, its repository gone): it stops waiting. Returns the
    /// execution it waited on, whose [`Fate`] may have changed. An execution's own leg ends through
    /// [`Demands::ended`] instead.
    pub fn forget(&mut self, leg: &str) -> Option<String> {
        if self.checking.remove(leg).is_some() {
            return None;
        }
        let exec = self.adopted.remove(leg)?;
        if let Some(x) = self.execs.get_mut(&exec) {
            x.waiters.retain(|w| w.leg != leg);
        }
        Some(exec)
    }

    /// Nothing executes, waits or checks.
    pub fn is_empty(&self) -> bool {
        self.execs.is_empty() && self.adopted.is_empty() && self.checking.is_empty() && self.inflight.is_empty()
    }

    /// An adopter whose execution it can't take over asks again.
    fn again(&mut self, w: &Waiter) -> Next {
        match self.ask(&w.leg, &w.run, &w.work, w.priority) {
            Ask::Adopt { exec, run } => Next::Adopt { exec, run },
            Ask::Execute => Next::Execute,
            Ask::Drop => Next::Drop,
        }
    }

    /// The execution in flight a leg doing `work` may adopt: its own domain's, then a trusted one.
    fn adoptable(&self, work: &Work) -> Option<String> {
        let key = work.key.as_ref()?;
        [&work.domain, &Domain::Trusted].into_iter().filter(|d| work.domain.may_adopt(d)).find_map(|d| self.inflight.get(&(key.clone(), d.clone())).cloned())
    }

    /// `w` adopts `exec`; returns `exec`'s run.
    fn attach(&mut self, exec: &str, w: Waiter) -> String {
        let Some(x) = self.execs.get_mut(exec) else { return String::new() };
        self.adopted.insert(w.leg.clone(), exec.to_owned());
        x.waiters.push(w);
        x.run.clone()
    }

    /// `leg` executes `work`: the in-flight table's for its key and domain, when its domain may
    /// adopt its own (protected's never is).
    fn register(&mut self, leg: &str, run: &str, work: &Work, priority: u8, started: Option<i64>) {
        let shared = match &work.key {
            Some(key) if work.domain.may_adopt(&work.domain) => match self.inflight.entry((key.clone(), work.domain.clone())) {
                Entry::Vacant(e) => {
                    e.insert(leg.to_owned());
                    true
                }
                Entry::Occupied(_) => false,
            },
            _ => false,
        };
        self.execs.insert(leg.to_owned(), Exec { run: run.to_owned(), work: work.clone(), priority, started, waiters: Vec::new(), shared });
    }

    fn unregister(&mut self, leg: &str, x: &Exec) {
        if x.shared
            && let Some(key) = &x.work.key
        {
            let slot = (key.clone(), x.work.domain.clone());
            if self.inflight.get(&slot).is_some_and(|l| l == leg) {
                self.inflight.remove(&slot);
            }
        }
    }
}

#[cfg(test)]
mod tests;
