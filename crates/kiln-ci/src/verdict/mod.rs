//! Verdicts per commit of a lane (docs/ci.md W28-R50 to R53), the pure part: for each commit of a
//! workflow's lane, per unit and per test, whether a result of its own answers it (direct), a later
//! commit's does (covered: optimistic, never passed), it may have broken something (suspect), or a
//! culprit finding named it (culprit). The server, and step 1's replayer, give the lane's commits
//! with what each one's diff could affect, the results and W16's marks; this says each commit's
//! verdict, and why.
//!
//! - A [`Lane`] is a workflow's commits on one branch, first parents, oldest first. Each carries the
//!   units its diff could affect (W24-R17, as [`crate::land::conflict::Units::affected`] gives
//!   them), or every unit when that isn't known (R50).
//! - A [`Check`] is a workflow's test of a unit, or the unit (a job, when results are by job) as a
//!   whole, with the checks it needs ([`Spec`]). A result ([`Seen`]) is its outcome at a commit,
//!   how it came and when (R50).
//! - A commit's check is direct when it has a result of its own. Else it is covered by the nearest
//!   later commit with one, when that one passed, failed as the base (the nearest earlier result)
//!   already failed, or failed a test W16 marks flaky or quarantined; suspect when that one newly
//!   failed it; pending while no later commit has a result (R51).
//! - A new failure's suspects are exactly the commits from just after its base to the failing one
//!   whose diffs could affect the check's unit ([`Failure`]). A culprit finding's outcome
//!   ([`Found`]) names the culprit among them and clears the rest, or finds the failure was a flake
//!   (R52).
//! - Checks a commit's diff can't affect are unaffected and never count against it; a check whose
//!   need fails at a commit is settled with that need. A commit's verdict is the weakest of its
//!   affected checks' (R50).
//!
//! [`Book`] keeps a lane as its results come, and says which commits' verdicts a new result can
//! change, so a replay or the server recomputes only those. Pure and deterministic, as docs/ci.md
//! §0.3 asks: the same lane, results, marks and findings give the same verdicts, whatever order the
//! results came in.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Range;

use serde::{Deserialize, Serialize};

pub mod cargo;

/// What a check tests: a workflow's test of a unit, or with no test the unit (or job) as a whole
/// (W28-R50).
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Check {
    pub workflow: String,
    pub unit: String,
    /// The test, by W16's id or name; empty for the unit as a whole.
    pub test: String,
}

impl Check {
    /// The unit as a whole, or a job when results are by job.
    pub fn unit(workflow: &str, unit: &str) -> Check {
        Check { workflow: workflow.to_owned(), unit: unit.to_owned(), test: String::new() }
    }
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.test.is_empty() { write!(f, "{}: {}", self.workflow, self.unit) } else { write!(f, "{}: {} ({})", self.workflow, self.test, self.unit) }
    }
}

/// A check of a lane, with the checks it needs: a job's `needs`, when checks are jobs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Spec {
    pub check: Check,
    pub needs: Vec<Check>,
}

/// The units a commit's diff could affect (W24-R17).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Affects {
    /// Every unit: its diff touched what no unit takes, or what it touched isn't known.
    #[default]
    All,
    Units(BTreeSet<String>),
}

impl Affects {
    pub fn unit(&self, unit: &str) -> bool {
        match self {
            Affects::All => true,
            Affects::Units(u) => u.contains(unit),
        }
    }
}

/// A commit of a lane: its id (a SHA, or what the caller knows it by), when it was pushed, and what
/// its diff could affect.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Commit {
    pub id: String,
    pub at: i64,
    pub affects: Affects,
}

/// A workflow's commits on one branch, first parents, oldest first, and its checks (W28-R50).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Lane {
    pub commits: Vec<Commit>,
    pub checks: Vec<Spec>,
}

/// How a check ended at a commit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Outcome {
    Passed,
    /// It failed; `signature` says how (the test, or for a job its failed step): two failures with
    /// one signature are the same failure.
    Failed { signature: String },
    /// It didn't run: a check it needs failed.
    Skipped,
}

/// How a result came to its commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum How {
    /// An execution of its own.
    Ran,
    /// A finished result with the same inputs.
    Reused,
    /// An execution in flight with the same inputs.
    Adopted,
}

/// A result: a check's outcome at a commit (its place in the lane), how it came and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    pub commit: usize,
    pub check: Check,
    pub outcome: Outcome,
    pub how: How,
    pub at: i64,
}

/// What a culprit finding over a new failure's suspects found (W28-R52).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "finding", rename_all = "kebab-case")]
pub enum Finding {
    /// This suspect (its place in the lane) broke it.
    Culprit { commit: usize },
    /// The failure didn't come back: a flake.
    NoCulprit,
    /// It couldn't tell, and says why.
    Inconclusive { why: String },
}

/// A culprit finding's outcome: the check, the suspects it searched (places in the lane), what it
/// found, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Found {
    pub check: Check,
    pub suspects: Vec<usize>,
    pub finding: Finding,
    pub at: i64,
}

/// A commit's verdict, from the strongest to the weakest (W28-R50).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// Every check its diff could affect has a result of its own (ran, reused or adopted), or its
    /// diff could affect none.
    Direct,
    /// Some are covered by later commits' results or cleared by a culprit finding: optimistic,
    /// never passed.
    Covered,
    /// Some wait for a result.
    Pending,
    /// Some newly failed, at it or a later commit, and its diff could have done it.
    Suspect,
    /// A culprit finding named it.
    Culprit,
}

impl Kind {
    /// Whether it answers the commit: direct, covered or culprit, not pending or suspect.
    pub fn answered(self) -> bool {
        matches!(self, Kind::Direct | Kind::Covered | Kind::Culprit)
    }
}

/// One check's verdict at a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// A result of its own.
    Direct,
    /// A later commit's result answers it: optimistic.
    Covered,
    /// A culprit finding cleared it: optimistic.
    Cleared,
    Pending,
    Suspect,
    Culprit,
    /// Its diff can't affect the check's unit: not its to answer.
    Unaffected,
}

impl Status {
    /// The commit's kind this status makes, when the check counts.
    fn kind(self) -> Kind {
        match self {
            Status::Direct | Status::Unaffected => Kind::Direct,
            Status::Covered | Status::Cleared => Kind::Covered,
            Status::Pending => Kind::Pending,
            Status::Suspect => Kind::Suspect,
            Status::Culprit => Kind::Culprit,
        }
    }
}

/// One check's verdict at a commit, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckVerdict {
    pub check: Check,
    pub status: Status,
    /// Its own result's outcome, for a direct check only: a covered check has none, so it is never
    /// shown as passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub how: Option<How>,
    /// Whether it's taken to fail at this commit: its own failure, a failure covering it, a suspect.
    pub failing: bool,
    /// Covered or cleared: from other commits' results, never this one's.
    pub optimistic: bool,
    /// The commit whose result it rests on: the covering one, or the failing one for a suspect, a
    /// culprit or one cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<usize>,
    /// The base: the nearest earlier commit with a result for the check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<usize>,
    /// The check it needs whose verdict it is settled with, failing at this commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub need: Option<Check>,
    /// When it came to this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
    pub why: String,
}

/// A commit's verdict: the weakest of its affected checks', each check's, and why (W28-R50).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub commit: usize,
    pub kind: Kind,
    /// When it came to its kind; none while pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
    pub checks: Vec<CheckVerdict>,
    pub why: String,
}

/// A new failure (W28-R52): the check failed at `commit` where it passed, or had no result, at the
/// base. Its suspects are the commits from just after the base to it whose diffs could affect the
/// check's unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub check: Check,
    pub commit: usize,
    pub base: Option<usize>,
    pub suspects: Vec<usize>,
}

/// The verdicts of `lane`'s commits from `seen`, W16's `marks` (a check's flaky or quarantine mark:
/// its failures don't count) and culprit findings' outcomes (W28-R50 to R52).
pub fn verdicts(lane: &Lane, seen: &[Seen], marks: &BTreeMap<Check, String>, found: &[Found]) -> Vec<Verdict> {
    let mut book = Book::new(lane.clone());
    for (check, why) in marks {
        book.mark(check.clone(), why.clone());
    }
    for s in seen {
        book.see(s.clone());
    }
    for f in found {
        book.found(f.clone());
    }
    book.verdicts()
}

/// A lane as its results come (W28-R50 to R52): each addition says which commits' verdicts it can
/// change.
#[derive(Debug, Clone, Default)]
pub struct Book {
    commits: Vec<Commit>,
    specs: Vec<Spec>,
    index: BTreeMap<Check, usize>,
    /// Each check's needs, by place.
    needs: Vec<Vec<usize>>,
    /// The checks, what each needs before it.
    order: Vec<usize>,
    /// Each check's result at each commit: the latest.
    own: Vec<BTreeMap<usize, Seen>>,
    /// The commits where each check's result tested it (not a skip).
    tested: Vec<BTreeSet<usize>>,
    marks: BTreeMap<Check, String>,
    /// Each check's culprit findings, as they came.
    found: Vec<Vec<Found>>,
}

impl Book {
    pub fn new(lane: Lane) -> Book {
        let mut b = Book::default();
        for s in lane.checks {
            b.spec(s);
        }
        for c in lane.commits {
            b.push(c);
        }
        b
    }

    pub fn len(&self) -> usize {
        self.commits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commits.is_empty()
    }

    pub fn commits(&self) -> &[Commit] {
        &self.commits
    }

    pub fn checks(&self) -> impl Iterator<Item = &Check> {
        self.specs.iter().map(|s| &s.check)
    }

    /// Adds a commit after the others; its place in the lane.
    pub fn push(&mut self, commit: Commit) -> usize {
        self.commits.push(commit);
        self.commits.len() - 1
    }

    /// Adds a check, or what more it needs; the commits whose verdicts that can change.
    pub fn spec(&mut self, spec: Spec) -> Range<usize> {
        let s = self.place(&spec.check);
        let mut changed = false;
        for n in &spec.needs {
            let k = self.place(n);
            if k != s && self.needs.get(s).is_some_and(|v| !v.contains(&k)) {
                if let Some(v) = self.needs.get_mut(s) {
                    v.push(k);
                }
                changed = true;
            }
        }
        if changed {
            self.reorder();
        }
        0..self.commits.len()
    }

    /// Marks a check as W16 does (flaky, probably flaky, quarantined): its failures don't count.
    pub fn mark(&mut self, check: Check, why: String) -> Range<usize> {
        self.place(&check);
        self.marks.insert(check, why);
        0..self.commits.len()
    }

    /// Adds a result; the commits whose verdicts it can change: those from just after the check's
    /// previous result to its next one.
    pub fn see(&mut self, seen: Seen) -> Range<usize> {
        let c = seen.commit;
        if c >= self.commits.len() {
            return 0..0;
        }
        let known = self.index.contains_key(&seen.check);
        let s = self.place(&seen.check);
        let newer = self.own.get(s).and_then(|m| m.get(&c)).is_none_or(|old| (seen.at, &seen.outcome, seen.how) > (old.at, &old.outcome, old.how));
        if !newer {
            return c..c;
        }
        let lo = self.prev(s, c).map_or(0, |p| p + 1);
        let hi = self.next(s, c).map_or(self.commits.len(), |n| n + 1);
        let tests = seen.outcome != Outcome::Skipped;
        if let Some(t) = self.tested.get_mut(s) {
            if tests {
                t.insert(c);
            } else {
                t.remove(&c);
            }
        }
        if let Some(m) = self.own.get_mut(s) {
            m.insert(c, seen);
        }
        if known { lo..hi } else { 0..self.commits.len() }
    }

    /// Adds a culprit finding's outcome; the commits whose verdicts it can change.
    pub fn found(&mut self, found: Found) -> Range<usize> {
        let s = self.place(&found.check);
        let lo = found.suspects.iter().copied().min().unwrap_or(0);
        let hi = found.suspects.iter().copied().max().map_or(0, |m| m + 1).min(self.commits.len());
        if let Some(v) = self.found.get_mut(s) {
            v.push(found);
        }
        lo.min(hi)..hi
    }

    /// Every commit's verdict, in lane order.
    pub fn verdicts(&self) -> Vec<Verdict> {
        (0..self.commits.len()).filter_map(|c| self.verdict(c)).collect()
    }

    /// One commit's verdict.
    pub fn verdict(&self, c: usize) -> Option<Verdict> {
        let commit = self.commits.get(c)?;
        let mut done: BTreeMap<usize, CheckVerdict> = BTreeMap::new();
        for &s in &self.order {
            let v = self.check_at(s, c, &done);
            done.insert(s, v);
        }
        // By check, so the order the checks were first seen in doesn't show.
        let mut checks: Vec<CheckVerdict> = done.into_values().collect();
        checks.sort_by(|a, b| a.check.cmp(&b.check));
        Some(summary(c, commit, checks, &self.commits))
    }

    /// The new failure at commit `c` for `check`, if its result there is one (W28-R52).
    pub fn failure(&self, check: &Check, c: usize) -> Option<Failure> {
        let s = *self.index.get(check)?;
        let Outcome::Failed { signature } = &self.own.get(s)?.get(&c)?.outcome else { return None };
        if self.marks.contains_key(check) {
            return None;
        }
        // New only where it passed at the base: with no earlier result there's nothing to say it did.
        let base = self.prev(s, c)?;
        if self.result(s, base).is_some_and(|r| failed_as(&r.outcome, signature)) {
            return None;
        }
        Some(Failure { check: check.clone(), commit: c, base: Some(base), suspects: self.suspects(s, Some(base), c) })
    }

    /// Every new failure, by check, then commit.
    pub fn failures(&self) -> Vec<Failure> {
        let mut out = Vec::new();
        for spec in &self.specs {
            let Some(&s) = self.index.get(&spec.check) else { continue };
            for &c in self.tested.get(s).into_iter().flatten() {
                out.extend(self.failure(&spec.check, c));
            }
        }
        out
    }

    /// A check's place, added with no needs if it's new.
    fn place(&mut self, check: &Check) -> usize {
        if let Some(&s) = self.index.get(check) {
            return s;
        }
        let s = self.specs.len();
        self.specs.push(Spec { check: check.clone(), needs: Vec::new() });
        self.index.insert(check.clone(), s);
        self.needs.push(Vec::new());
        self.own.push(BTreeMap::new());
        self.tested.push(BTreeSet::new());
        self.found.push(Vec::new());
        self.order.push(s);
        s
    }

    /// The checks in an order that puts what each needs before it; a cycle is cut where it closes.
    fn reorder(&mut self) {
        fn visit(s: usize, needs: &[Vec<usize>], state: &mut [u8], order: &mut Vec<usize>) {
            if state.get(s).copied() != Some(0) {
                return;
            }
            if let Some(x) = state.get_mut(s) {
                *x = 1;
            }
            for &n in needs.get(s).into_iter().flatten() {
                visit(n, needs, state, order);
            }
            if let Some(x) = state.get_mut(s) {
                *x = 2;
            }
            order.push(s);
        }
        let mut state = vec![0u8; self.specs.len()];
        let mut order = Vec::with_capacity(self.specs.len());
        for s in 0..self.specs.len() {
            visit(s, &self.needs, &mut state, &mut order);
        }
        self.order = order;
    }

    fn result(&self, s: usize, c: usize) -> Option<&Seen> {
        self.own.get(s).and_then(|m| m.get(&c))
    }

    /// The nearest commit before `c` whose result tested the check: the base.
    fn prev(&self, s: usize, c: usize) -> Option<usize> {
        self.tested.get(s).and_then(|t| t.range(..c).next_back().copied())
    }

    /// The nearest commit after `c` whose result tested the check.
    fn next(&self, s: usize, c: usize) -> Option<usize> {
        self.tested.get(s).and_then(|t| t.range(c.saturating_add(1)..).next().copied())
    }

    fn affects(&self, s: usize, c: usize) -> bool {
        let unit = self.specs.get(s).map_or("", |x| x.check.unit.as_str());
        self.commits.get(c).is_some_and(|x| x.affects.unit(unit))
    }

    /// The commits from just after `base` to `d` whose diffs could affect the check's unit.
    fn suspects(&self, s: usize, base: Option<usize>, d: usize) -> Vec<usize> {
        (base.map_or(0, |b| b + 1)..=d).filter(|&c| self.affects(s, c)).collect()
    }

    fn id(&self, c: usize) -> String {
        self.commits.get(c).map_or_else(|| format!("#{c}"), |x| short(&x.id))
    }

    /// The check's verdict at `c`, the verdicts of what it needs there being `done`.
    fn check_at(&self, s: usize, c: usize, done: &BTreeMap<usize, CheckVerdict>) -> CheckVerdict {
        let check = self.specs.get(s).map(|x| x.check.clone()).unwrap_or_default();
        let affected = self.affects(s, c);
        let own = self.result(s, c);
        // Its own result tested it.
        if let Some(r) = own.filter(|r| r.outcome != Outcome::Skipped) {
            return self.own_result(s, c, r, affected);
        }
        let base = self.prev(s, c);
        // A later commit newly failed it, and this commit's diff could have: a suspect, whatever
        // its needs did here.
        if affected
            && let Some(d) = self.next(s, c)
            && self.result(s, d).is_some_and(|r| self.new_at(s, base, &r.outcome))
        {
            return self.suspect(s, c, d, base, None);
        }
        // A check it needs fails here: it is settled with that one. A skip with no failing need
        // says nothing of the check: as if it had no result here.
        if let Some(need) = self.needs.get(s).into_iter().flatten().filter_map(|n| done.get(n)).find(|v| v.failing) {
            return settled(check, need, affected, own);
        }
        if !affected {
            return unaffected(check, &self.id(c));
        }
        let Some(d) = self.next(s, c) else {
            let why = format!("pending: no result for it yet at {} or a later commit", self.id(c));
            return CheckVerdict { status: Status::Pending, base, why, ..blank(check) };
        };
        let Some(r) = self.result(s, d) else { return CheckVerdict { status: Status::Pending, base, why: "pending".into(), ..blank(check) } };
        let covered = |failing: bool, why: String| CheckVerdict { status: Status::Covered, failing, optimistic: true, by: Some(d), base, at: Some(r.at), why, ..blank(check.clone()) };
        match &r.outcome {
            Outcome::Passed => covered(false, format!("covered by {}, which passed it (optimistic: not run at {})", self.id(d), self.id(c))),
            Outcome::Failed { signature } => {
                if let Some(mark) = self.marks.get(&check) {
                    covered(true, format!("covered by {}: it failed there, but W16 marks it {mark}, so its failures don't count (optimistic)", self.id(d)))
                } else if let Some(b) = base.filter(|&b| self.result(s, b).is_some_and(|x| failed_as(&x.outcome, signature))) {
                    covered(true, format!("covered by {}: it failed there as it already did at the base, {} (optimistic)", self.id(d), self.id(b)))
                } else if base.is_none() {
                    covered(true, format!("covered by {}: it failed there, with no earlier result to say it ever passed (optimistic)", self.id(d)))
                } else {
                    self.suspect(s, c, d, base, None)
                }
            }
            Outcome::Skipped => CheckVerdict { status: Status::Pending, base, why: "pending".into(), ..blank(check) },
        }
    }

    /// Whether `outcome`, a result after `base`, is a new failure: it failed, the check isn't
    /// marked, and the base passed or failed otherwise.
    fn new_at(&self, s: usize, base: Option<usize>, outcome: &Outcome) -> bool {
        let Outcome::Failed { signature } = outcome else { return false };
        let marked = self.specs.get(s).is_some_and(|x| self.marks.contains_key(&x.check));
        !marked && base.and_then(|b| self.result(s, b)).is_some_and(|r| !failed_as(&r.outcome, signature))
    }

    /// The check's verdict at `c` from its own result `r`, which tested it.
    fn own_result(&self, s: usize, c: usize, r: &Seen, affected: bool) -> CheckVerdict {
        let check = self.specs.get(s).map(|x| x.check.clone()).unwrap_or_default();
        let base = self.prev(s, c);
        let status = if affected { Status::Direct } else { Status::Unaffected };
        let direct = |failing: bool, why: String| CheckVerdict { status, outcome: Some(r.outcome.clone()), how: Some(r.how), failing, base, at: Some(r.at), why, ..blank(check.clone()) };
        let how = match r.how {
            How::Ran => "ran",
            How::Reused => "reused",
            How::Adopted => "adopted",
        };
        match &r.outcome {
            Outcome::Failed { signature } => {
                if let Some(mark) = self.marks.get(&check) {
                    direct(true, format!("failed at {} ({how}), but W16 marks it {mark}, so its failures don't count", self.id(c)))
                } else if let Some(b) = base.filter(|&b| self.result(s, b).is_some_and(|x| failed_as(&x.outcome, signature))) {
                    direct(true, format!("failed at {} ({how}) as it already did at the base, {}", self.id(c), self.id(b)))
                } else if base.is_none() {
                    direct(true, format!("failed at {} ({how}), with no earlier result to say it ever passed", self.id(c)))
                } else if !affected {
                    direct(true, format!("newly failed at {} ({how}), but its diff can't affect {}: an earlier commit's did", self.id(c), check.unit))
                } else {
                    self.suspect(s, c, c, base, Some(r))
                }
            }
            _ if !affected => direct(false, format!("unaffected: its diff can't affect {}; it passed at {} ({how})", check.unit, self.id(c))),
            _ => direct(false, format!("passed at {} ({how})", self.id(c))),
        }
    }

    /// The check's verdict at `c`, a suspect of the new failure at `d` (its own when `own`): as the
    /// latest culprit finding whose suspects include it says, else suspect.
    fn suspect(&self, s: usize, c: usize, d: usize, base: Option<usize>, own: Option<&Seen>) -> CheckVerdict {
        let check = self.specs.get(s).map(|x| x.check.clone()).unwrap_or_default();
        // The latest; at one time, by what they found, so the order findings came in doesn't show.
        let finding = self.found.get(s).and_then(|v| v.iter().filter(|f| f.suspects.contains(&c)).max_by(|a, b| (a.at, &a.finding, &a.suspects).cmp(&(b.at, &b.finding, &b.suspects))));
        let at_base = base.map_or_else(|| "it had no earlier result".to_owned(), |b| format!("it passed at the base, {}", self.id(b)));
        let mut v = CheckVerdict { status: Status::Suspect, outcome: own.map(|r| r.outcome.clone()), how: own.map(|r| r.how), failing: true, by: Some(d), base, at: Some(own.map_or_else(|| self.result(s, d).map_or(0, |r| r.at), |r| r.at)), why: String::new(), ..blank(check.clone()) };
        let suspect_why = format!("suspect: newly failed at {} ({at_base}), and the diff of {} could affect {}", self.id(d), self.id(c), check.unit);
        let Some(f) = finding else {
            v.why = suspect_why;
            return v;
        };
        let direct = own.is_some();
        match &f.finding {
            Finding::Culprit { commit: x } if *x == c => {
                v.status = Status::Culprit;
                v.at = Some(f.at);
                v.why = format!("culprit: the culprit finding named it for the failure at {} ({at_base})", self.id(d));
            }
            Finding::Culprit { commit: x } if c < *x => {
                v.status = if direct { Status::Direct } else { Status::Cleared };
                (v.failing, v.optimistic, v.at) = (false, !direct, Some(f.at));
                v.why = format!("cleared: the culprit finding named {}, after it", self.id(*x));
            }
            Finding::Culprit { commit: x } => {
                v.status = if direct { Status::Direct } else { Status::Cleared };
                (v.optimistic, v.at) = (!direct, Some(f.at));
                v.why = format!("after the culprit {}: it fails as {} made it", self.id(*x), self.id(*x));
            }
            Finding::NoCulprit => {
                v.status = if direct { Status::Direct } else { Status::Cleared };
                (v.failing, v.optimistic, v.at) = (direct, !direct, Some(f.at));
                v.why = format!("cleared: its failure at {} didn't come back when tried again: a flake", self.id(d));
            }
            Finding::Inconclusive { why } => v.why = format!("{suspect_why}; the culprit finding couldn't tell: {why}"),
        }
        v
    }
}

/// Whether `outcome` is a failure with `signature`.
fn failed_as(outcome: &Outcome, signature: &str) -> bool {
    matches!(outcome, Outcome::Failed { signature: s } if s == signature)
}

/// A check's verdict before it is decided.
fn blank(check: Check) -> CheckVerdict {
    CheckVerdict { check, status: Status::Pending, outcome: None, how: None, failing: false, optimistic: false, by: None, base: None, need: None, at: None, why: String::new() }
}

fn unaffected(check: Check, id: &str) -> CheckVerdict {
    let why = format!("unaffected: the diff of {id} can't affect {}", check.unit);
    CheckVerdict { status: Status::Unaffected, why, ..blank(check) }
}

/// A check settled with a need that fails at its commit: it takes that need's verdict.
fn settled(check: Check, need: &CheckVerdict, affected: bool, own: Option<&Seen>) -> CheckVerdict {
    let status = match need.status {
        _ if !affected => Status::Unaffected,
        Status::Unaffected => Status::Direct,
        s => s,
    };
    let outcome = matches!(status, Status::Direct | Status::Unaffected).then_some(Outcome::Skipped);
    let why = format!("settled with {}, which it needs: {}", need.check, need.why);
    CheckVerdict { status, outcome, how: own.map(|r| r.how), failing: true, optimistic: need.optimistic, by: need.by, base: need.base, need: Some(need.check.clone()), at: own.map(|r| r.at).or(need.at), why, ..blank(check) }
}

/// A commit's verdict from its checks' (W28-R50).
fn summary(c: usize, commit: &Commit, checks: Vec<CheckVerdict>, commits: &[Commit]) -> Verdict {
    let counted: Vec<&CheckVerdict> = checks.iter().filter(|v| v.status != Status::Unaffected).collect();
    let kind = counted.iter().map(|v| v.status.kind()).max().unwrap_or(Kind::Direct);
    // Answered: when its last check was. Suspect or culprit: when the checks that make it so were.
    let at = match kind {
        Kind::Pending => None,
        Kind::Suspect | Kind::Culprit => Some(counted.iter().filter(|v| v.status.kind() == kind).filter_map(|v| v.at).max().unwrap_or(commit.at).max(commit.at)),
        Kind::Direct | Kind::Covered => Some(counted.iter().filter_map(|v| v.at).max().unwrap_or(commit.at).max(commit.at)),
    };
    let of = |st: &[Status]| counted.iter().filter(|v| st.contains(&v.status)).count();
    let first = |st: Status| counted.iter().find(|v| v.status == st);
    let more = |n: usize| if n > 1 { format!(" and {} more", n - 1) } else { String::new() };
    let id = |d: Option<usize>| d.and_then(|d| commits.get(d)).map_or_else(String::new, |x| short(&x.id));
    let why = match kind {
        Kind::Direct if counted.is_empty() => "direct: its diff affects none of the lane's checks".to_owned(),
        Kind::Direct => format!("direct: all {} checks its diff could affect have results of its own", counted.len()),
        Kind::Covered => format!("covered (optimistic): {} of the {} checks its diff could affect are answered by later commits or a culprit finding, the rest by its own results", of(&[Status::Covered, Status::Cleared]), counted.len()),
        Kind::Pending => {
            let n = of(&[Status::Pending]);
            format!("pending: {}{} wait for a result", first(Status::Pending).map_or_else(String::new, |v| v.check.to_string()), more(n))
        }
        Kind::Suspect => {
            let n = of(&[Status::Suspect]);
            first(Status::Suspect).map_or_else(|| "suspect".to_owned(), |v| format!("suspect: {} newly failed at {}{}, and its diff could have done it", v.check, id(v.by), more(n)))
        }
        Kind::Culprit => {
            let n = of(&[Status::Culprit]);
            first(Status::Culprit).map_or_else(|| "culprit".to_owned(), |v| format!("culprit: named for {}, which newly failed at {}{}", v.check, id(v.by), more(n)))
        }
    };
    Verdict { commit: c, kind, at, checks, why }
}

/// A commit's id as verdicts say it: a SHA's first 12 digits.
fn short(id: &str) -> String {
    if id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit()) { id.get(..12).unwrap_or(id).to_owned() } else { id.to_owned() }
}

#[cfg(test)]
mod tests;
