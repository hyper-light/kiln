//! Verdicts per commit (docs/ci.md W28-R55), the server's part: a lane's commits from the mirror,
//! what each one's diff could affect (its changed files over the Cargo workspace's units,
//! `kiln_ci::verdict::cargo`), W16's test results as checks (each test's unit from its file, else
//! its suite's crate, else its job; legs without reports as their jobs), W16's flaky marks and
//! quarantine as failures that don't count. `kiln_ci::verdict` decides; this only reads.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::StatusCode;
use kiln_ci::land::conflict::Units;
use kiln_ci::verdict::{Affects, Book, Check, Commit, Finding, Found, How, Kind, Outcome, Seen, Verdict};
use serde_json::{Value, json};

use super::Fail;
use super::bisect::Bisect;
use crate::hub::Hub;

/// The most commits a lane looks back over, from the commit asked about.
const BACK: usize = 200;

fn fail(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

pub(crate) async fn git(hub: &Hub, args: &[&str]) -> Option<String> {
    crate::gitops::run_git(hub, args, crate::gitops::LS_REMOTE_TIMEOUT).await.ok().map(|o| String::from_utf8_lossy(&o).into_owned())
}

/// One run of the lane: its id, commit, attempt.
struct RunRow {
    id: String,
    sha: String,
    attempt: i64,
}

/// `GET /api/ci/repos/:repo/commits/:sha/verdict?workflow=` (`ci.verdict`): the commit's verdict on
/// each lane (workflow and branch) a run of it is on, or `workflow`'s.
pub async fn of_commit(hub: &Hub, repo_key: &str, sha: &str, q: &HashMap<String, String>) -> Result<Value, Fail> {
    let repo = hub.store.repos().await.map_err(fail)?.into_iter().find(|r| r.name == repo_key || r.id == repo_key).ok_or((StatusCode::NOT_FOUND, "no such repository".into()))?;
    let mirror = crate::gitops::mirror_path(hub, &repo.name).to_string_lossy().into_owned();
    let full = git(hub, &["-C", &mirror, "rev-parse", "--verify", "--quiet", &format!("{sha}^{{commit}}")]).await.map(|s| s.trim().to_owned()).filter(|s| s.len() == 40).ok_or((StatusCode::NOT_FOUND, format!("no commit {sha} in {}", repo.name)))?;
    // Every lane of the repository: those whose first parents hold the commit answer for it.
    let id = repo.id.clone();
    let lanes: Vec<(String, String)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT DISTINCT workflow_id, branch FROM runs WHERE repo_id = ?1 AND COALESCE(workflow_id, '') != '' ORDER BY workflow_id, branch")?;
            let rows = st.query_map([&id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(fail)?;
    let only = q.get("workflow").filter(|w| !w.is_empty());
    let mut out = Vec::new();
    for (workflow, branch) in lanes.into_iter().filter(|(w, _)| only.is_none_or(|o| o == w)) {
        if let Some(v) = lane(hub, (&repo.id, &repo.name), &mirror, &workflow, &branch, &full).await? {
            out.push(json!({ "workflow": workflow, "branch": branch, "verdict": v }));
        }
    }
    Ok(json!({ "repo": repo.name, "sha": full, "lanes": out }))
}

/// The commit's verdict on one lane, if the lane's first parents hold it.
async fn lane(hub: &Hub, repo: (&str, &str), mirror: &str, workflow: &str, branch: &str, sha: &str) -> Result<Option<Value>, Fail> {
    let Some(l) = lane_book(hub, repo, mirror, workflow, branch, sha).await? else { return Ok(None) };
    let Some(p) = l.shas.iter().position(|s| s == sha) else { return Ok(None) };
    Ok(l.book.verdict(p).map(|v| l.named(&v)))
}

/// A lane as `kiln_ci::verdict` keeps it: its commits by SHA, the places of those with runs, and
/// its book.
struct LaneBook {
    shas: Vec<String>,
    runs: BTreeSet<usize>,
    book: Book,
}

impl LaneBook {
    /// A verdict as JSON, its commits by SHA.
    fn named(&self, v: &kiln_ci::verdict::Verdict) -> Value {
        let mut j = serde_json::to_value(v).unwrap_or(Value::Null);
        named(&mut j, &self.shas);
        j
    }
}

/// The lane of `workflow` on `branch`, from its tip back to `sha` and BACK more, if its first
/// parents hold `sha`.
async fn lane_book(hub: &Hub, (repo_id, repo): (&str, &str), mirror: &str, workflow: &str, branch: &str, sha: &str) -> Result<Option<LaneBook>, Fail> {
    // The lane's commits: first parents up to the branch's tip, back to the commit and BACK more.
    let (r, w, b) = (repo_id.to_owned(), workflow.to_owned(), branch.to_owned());
    let runs: Vec<RunRow> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT id, sha, attempt FROM runs WHERE repo_id = ?1 AND workflow_id = ?2 AND branch = ?3 ORDER BY created_at")?;
            let rows = st.query_map(rusqlite::params![r, w, b], |x| Ok(RunRow { id: x.get(0)?, sha: x.get(1)?, attempt: x.get(2)? }))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(fail)?;
    // The branch's tip; a try a bisection ran (at an older commit) may be the newest run.
    let tip = git(hub, &["-C", mirror, "rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).await.map(|s| s.trim().to_owned()).filter(|s| s.len() == 40);
    let head = tip.unwrap_or_else(|| runs.last().map_or_else(|| sha.to_owned(), |r| r.sha.clone()));
    let listed = git(hub, &["-C", mirror, "rev-list", "--first-parent", &format!("--max-count={}", BACK * 2), &head]).await.unwrap_or_default();
    let mut shas: Vec<String> = listed.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect();
    shas.reverse();
    let Some(at) = shas.iter().position(|s| s == sha) else { return Ok(None) };
    let start = at.saturating_sub(BACK);
    let shas: Vec<String> = shas.into_iter().skip(start).collect();
    // The workspace's units, as the newest commit has them.
    let units = Arc::new(units_at(hub, mirror, &head).await);
    // Each commit's diff against its first parent, read in the lane's order; the first's isn't read, as it affects every
    // unit. What each diff could affect is worked out as the book is made.
    let mut diffs: Vec<Option<String>> = Vec::with_capacity(shas.len());
    for (i, s) in shas.iter().enumerate() {
        diffs.push(if i == 0 { None } else { diff_of(hub, mirror, s).await });
    }
    // The lane's runs at its commits, with their places, picked out on the compute pool, off the shards (goal 9, plan
    // 01a1a87b2cec step 3): the runs read are every one the lane ever had, thousands on a busy branch.
    let (shas, at_commits, with_runs) = crate::rt::compute(move || {
        let (at_commits, with_runs) = runs_at(&shas, runs);
        (shas, at_commits, with_runs)
    })
    .await
    .map_err(fail)?;
    // Each one's results and W16's marks on its tests, read a run at a time, in the runs' order.
    let mut said: Vec<Said> = Vec::with_capacity(at_commits.len());
    for (p, run) in &at_commits {
        let seen = results(hub, repo_id, workflow, &run.id, run.attempt, &units, *p).await?;
        said.push((seen, marks(hub, &run.id, workflow, &units).await));
    }
    // W28-R57: the lane's culprit findings, its bisections that ended.
    let bisections = super::bisect::of_lane(hub, repo, workflow, branch).await;
    // The book made of it all on the compute pool, in pieces, off the shards (goal 9, plan 01a1a87b2cec step 3): each
    // result it takes costs a microsecond or two, and a run has one for each of its tests, thousands of them.
    let (shas, book) = made(Making::new(units, workflow, shas, diffs, said, bisections)).await.map_err(fail)?;
    Ok(Some(LaneBook { shas, runs: with_runs, book }))
}

/// Of a lane's runs, every one it ever had in the order they were made, those at its commits `shas` with their commits'
/// places, in that order; and the places with runs.
fn runs_at(shas: &[String], runs: Vec<RunRow>) -> (Vec<(usize, RunRow)>, BTreeSet<usize>) {
    let place: BTreeMap<&str, usize> = shas.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();
    let at: Vec<(usize, RunRow)> = runs.into_iter().filter_map(|r| place.get(r.sha.as_str()).map(|&p| (p, r))).collect();
    let with = at.iter().map(|(p, _)| *p).collect();
    (at, with)
}

/// A run's results and W16's marks on its tests, as [`results`] and [`marks`] read them.
type Said = (Vec<Seen>, Vec<(Check, String)>);

/// A lane's book in the making (goal 9, plan 01a1a87b2cec step 3): what [`lane_book`] read for it, taken on the compute
/// pool a commit, result, mark or finding at a time, in the order the book took them as the reads came (the commits,
/// then each run's results and marks, run by run, then the findings), so it's the same book.
struct Making {
    units: Arc<Units>,
    workflow: String,
    shas: Vec<String>,
    /// Each commit's diff, by place: none for the first, which affects every unit, or for one that couldn't be read.
    diffs: std::iter::Enumerate<std::vec::IntoIter<Option<String>>>,
    runs: std::vec::IntoIter<Said>,
    /// The run being taken: its results, then its marks.
    seen: std::vec::IntoIter<Seen>,
    marks: std::vec::IntoIter<(Check, String)>,
    bisections: std::vec::IntoIter<Bisect>,
    /// The commits' places by SHA, for the findings: made as the first is taken.
    place: Option<BTreeMap<String, usize>>,
    book: Book,
}

impl Making {
    fn new(units: Arc<Units>, workflow: &str, shas: Vec<String>, diffs: Vec<Option<String>>, runs: Vec<Said>, bisections: Vec<Bisect>) -> Making {
        Making {
            units,
            workflow: workflow.to_owned(),
            shas,
            diffs: diffs.into_iter().enumerate(),
            runs: runs.into_iter(),
            seen: Vec::new().into_iter(),
            marks: Vec::new().into_iter(),
            bisections: bisections.into_iter(),
            place: None,
            book: Book::default(),
        }
    }

    /// Takes the next of what the book is made of, in its order: false once all of it is in.
    fn step(&mut self) -> bool {
        if let Some((i, diff)) = self.diffs.next() {
            let affects = if i == 0 { Affects::All } else { affects_of(&self.units, diff.as_deref()) };
            self.book.push(Commit { id: self.shas.get(i).cloned().unwrap_or_default(), at: 0, affects });
        } else if let Some(seen) = self.seen.next() {
            self.book.see(seen);
        } else if let Some((check, why)) = self.marks.next() {
            self.book.mark(check, why);
        } else if let Some((seen, marks)) = self.runs.next() {
            self.seen = seen.into_iter();
            self.marks = marks.into_iter();
        } else if let Some(b) = self.bisections.next() {
            if self.place.is_none() {
                self.place = Some(self.shas.iter().enumerate().map(|(i, s)| (s.clone(), i)).collect());
            }
            if let Some(found) = self.place.as_ref().and_then(|place| found_of(&b, place, &self.workflow)) {
                self.book.found(found);
            }
        } else {
            return false;
        }
        true
    }

    /// The lane's commits by SHA, and its book.
    fn done(&mut self) -> (Vec<String>, Book) {
        (std::mem::take(&mut self.shas), std::mem::take(&mut self.book))
    }
}

/// The book [`Making`] makes, made on the compute pool in pieces: a piece ends where another job waits for its thread
/// (`crate::rt::compute_pieces`), so a lane's thousands of results hold no pool thread long.
async fn made(making: Making) -> anyhow::Result<(Vec<String>, Book)> {
    crate::rt::compute_pieces(making, |m, turn| {
        while m.step() {
            if turn.over() {
                return crate::rt::Step::More;
            }
        }
        crate::rt::Step::Done(m.done())
    })
    .await
}

/// A bisection's finding (W28-R57), with the lane's commits by place: none for one still running or failed, or one whose
/// culprit isn't in the lane.
fn found_of(b: &Bisect, place: &BTreeMap<String, usize>, workflow: &str) -> Option<Found> {
    let suspects: Vec<usize> = b.suspects().iter().filter_map(|s| place.get(s.as_str()).copied()).collect();
    let finding = match (b.state.as_str(), &b.culprit) {
        ("found", Some(c)) => Finding::Culprit { commit: *place.get(c.sha.as_str())? },
        ("no-culprit", _) => Finding::NoCulprit,
        ("inconclusive", _) => Finding::Inconclusive { why: b.why.clone().unwrap_or_default() },
        _ => return None,
    };
    let check = match &b.test {
        Some(t) => Check { workflow: workflow.to_owned(), unit: b.unit.clone().unwrap_or_else(|| b.job.clone()), test: t.clone() },
        None => Check::unit(workflow, &b.job),
    };
    Some(Found { check, suspects, finding, at: b.updated_at })
}

/// A verdict as an agent reads it: each commit by its SHA, not its place in the lane.
fn named(v: &mut Value, shas: &[String]) {
    let sha = |x: &mut Value| {
        if let Some(s) = x.as_u64().and_then(|i| usize::try_from(i).ok()).and_then(|i| shas.get(i)) {
            *x = Value::String(s.clone());
        }
    };
    if let Some(c) = v.get_mut("commit") {
        sha(c);
    }
    for check in v.get_mut("checks").and_then(Value::as_array_mut).into_iter().flatten() {
        for k in ["by", "base"] {
            if let Some(x) = check.get_mut(k) {
                sha(x);
            }
        }
    }
}

/// The Cargo workspace's units at `sha` (W28-R54): none when it has no root manifest.
pub(crate) async fn units_at(hub: &Hub, mirror: &str, sha: &str) -> Units {
    let listed = git(hub, &["-C", mirror, "ls-tree", "-r", "--name-only", sha]).await.unwrap_or_default();
    let Some(root) = git(hub, &["-C", mirror, "show", &format!("{sha}:Cargo.toml")]).await else { return Units::default() };
    // The tree's files, and the members' manifests among them, listed on the compute pool, off the shards (goal 9, plan
    // 01a1a87b2cec step 3): a tree's lines, ten thousand of them, were made Strings in one poll of the shard.
    let Ok((files, members)) = crate::rt::compute(move || listing(&listed)).await else { return Units::default() };
    let mut manifests: BTreeMap<String, String> = BTreeMap::new();
    for f in members {
        if let Some(t) = git(hub, &["-C", mirror, "show", &format!("{sha}:{f}")]).await {
            manifests.insert(f, t);
        }
    }
    // The manifests parsed, and the units made, there too.
    crate::rt::compute(move || units_of(&root, &files, &manifests)).await.unwrap_or_default()
}

/// A tree's files as `git ls-tree --name-only` lists them, and the members' manifests among them (each `Cargo.toml` but
/// the root's), in its order.
fn listing(listed: &str) -> (Vec<String>, Vec<String>) {
    let files: Vec<String> = listed.lines().map(str::to_owned).collect();
    let members = files.iter().filter(|f| f.ends_with("Cargo.toml") && f.as_str() != "Cargo.toml").cloned().collect();
    (files, members)
}

/// The workspace's units from its root manifest, its tree's files and its members' manifests by path.
fn units_of(root: &str, files: &[String], manifests: &BTreeMap<String, String>) -> Units {
    Units::new(kiln_ci::verdict::cargo::units(root, files, |p| manifests.get(p).cloned()).unwrap_or_default())
}

/// What commit `sha`'s diff against its first parent could affect (W24-R17): a file no unit
/// takes, or a diff that can't be read, affects every unit.
pub(crate) async fn affects(hub: &Hub, mirror: &str, units: &Units, sha: &str) -> Affects {
    affects_of(units, diff_of(hub, mirror, sha).await.as_deref())
}

/// Commit `sha`'s changed files against its first parent, as `git diff-tree` lists them: none when they can't be read.
async fn diff_of(hub: &Hub, mirror: &str, sha: &str) -> Option<String> {
    git(hub, &["-C", mirror, "diff-tree", "--no-commit-id", "--name-only", "-r", &format!("{sha}^"), sha]).await
}

/// What a diff could affect, from its changed files as `git diff-tree` lists them (W24-R17): a file no unit takes, or a
/// diff that couldn't be read (none), affects every unit.
fn affects_of(units: &Units, changed: Option<&str>) -> Affects {
    let Some(changed) = changed else { return Affects::All };
    let files: Vec<&str> = changed.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if files.iter().any(|f| units.takes(f).is_empty()) { Affects::All } else { Affects::Units(units.affected(files)) }
}

/// A test's unit: its file's, else its suite's crate when a unit has that name, else its job.
pub(crate) fn unit_of(units: &Units, file: Option<&str>, suite: &str, job: &str) -> String {
    file.and_then(|f| units.takes(f).into_iter().next().map(str::to_owned))
        .or_else(|| suite.split([':', ' ', '/']).next().filter(|s| units.unit(s).is_some() || units.unit(&s.replace('_', "-")).is_some()).map(|s| units.unit(s).map_or_else(|| s.replace('_', "-"), |_| s.to_owned())))
        .unwrap_or_else(|| job.to_owned())
}

/// A run's test rows as [`results`] reads them: suite, name, file, leg, outcome and when.
type TestRow = (String, String, Option<String>, String, String, i64);
/// A run's job rows as [`results`] reads them: job, conclusion and when it finished.
type JobRow = (String, Option<String>, i64);

/// A run's results at lane place `p`: each test of its latest attempt, and each job without reports.
async fn results(hub: &Hub, repo_id: &str, workflow: &str, run: &str, attempt: i64, units: &Arc<Units>, p: usize) -> Result<Vec<Seen>, Fail> {
    let (r, rid) = (run.to_owned(), repo_id.to_owned());
    let (tests, jobs): (Vec<TestRow>, Vec<JobRow>) = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT k.suite, k.name, k.file, t.leg_key, t.outcome, COALESCE(t.ts, 0) FROM test_results t JOIN test_cases k ON k.repo_id = t.repo_id AND k.id = t.case_id WHERE t.run_id = ?1 AND t.repo_id = ?2 AND t.attempt = (SELECT MAX(attempt) FROM test_results WHERE run_id = ?1)")?;
            let tests = st.query_map(rusqlite::params![r, rid], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?, x.get(5)?)))?.collect::<Result<Vec<_>, _>>()?;
            let mut st = c.prepare("SELECT job_id, conclusion, COALESCE(finished_at, 0) FROM jobs WHERE run_id = ?1 AND job_id IS NOT NULL AND run_attempt = ?2")?;
            let jobs = st.query_map(rusqlite::params![r, attempt], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok((tests, jobs))
        })
        .await
        .map_err(fail)?;
    // Made results on the compute pool, off the shards (goal 9, plan 01a1a87b2cec step 3): a test's unit and name, a
    // microsecond or two each, were several milliseconds of one poll at thousands of tests.
    let (workflow, units) = (workflow.to_owned(), Arc::clone(units));
    crate::rt::compute(move || seen_of(&workflow, &units, p, tests, jobs)).await.map_err(fail)
}

/// A run's rows as its results at lane place `p` ([`results`]), on the thread it's called on: a compute job's.
fn seen_of(workflow: &str, units: &Units, p: usize, tests: Vec<TestRow>, jobs: Vec<JobRow>) -> Vec<Seen> {
    let reported: BTreeSet<String> = tests.iter().map(|t| t.3.split('/').next().unwrap_or(&t.3).to_owned()).collect();
    let mut out: Vec<Seen> = tests
        .iter()
        .filter_map(|(suite, name, file, leg, outcome, at)| {
            let job = leg.split('/').next().unwrap_or(leg);
            let o = match outcome.as_str() {
                "passed" => Outcome::Passed,
                "failed" | "error" => Outcome::Failed { signature: format!("{suite} {name}") },
                _ => return None,
            };
            Some(Seen { commit: p, check: Check { workflow: workflow.to_owned(), unit: unit_of(units, file.as_deref(), suite, job), test: format!("{suite} {name}").trim().to_owned() }, outcome: o, how: How::Ran, at: *at })
        })
        .collect();
    for (job, conclusion, at) in jobs {
        let job = job.split('/').next().unwrap_or(&job).to_owned();
        if reported.contains(&job) {
            continue;
        }
        let o = match conclusion.as_deref() {
            Some("success") => Outcome::Passed,
            Some("failure" | "timed_out") => Outcome::Failed { signature: conclusion.unwrap_or_default() },
            Some("skipped") => Outcome::Skipped,
            _ => continue,
        };
        out.push(Seen { commit: p, check: Check::unit(workflow, &job), outcome: o, how: How::Ran, at });
    }
    out
}

/// A run's test rows as [`marks`] reads them: suite, name, file, leg and whether it's quarantined.
type MarkRow = (String, String, Option<String>, String, i64);

/// W16's marks on a run's tests: flaky (W16-R04's first rule) and quarantined (W16-R05).
async fn marks(hub: &Hub, run: &str, workflow: &str, units: &Arc<Units>) -> Vec<(Check, String)> {
    let r = run.to_owned();
    let tests: Vec<MarkRow> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT k.suite, k.name, k.file, t.leg_key, t.quarantined FROM test_results t JOIN test_cases k ON k.repo_id = t.repo_id AND k.id = t.case_id WHERE t.run_id = ?1")?;
            let rows = st.query_map([&r], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap_or_default();
    let flaky = super::results::flaky_in_run(hub, run).await.unwrap_or_default();
    // Made marks on the compute pool, off the shards (goal 9, plan 01a1a87b2cec step 3), as results are.
    let (workflow, units) = (workflow.to_owned(), Arc::clone(units));
    crate::rt::compute(move || marks_of(&workflow, &units, tests, flaky)).await.unwrap_or_default()
}

/// A run's rows as W16's marks ([`marks`]), `flaky` the tests it was flaky on: sorted, one a check. On the thread it's
/// called on, a compute job's.
fn marks_of(workflow: &str, units: &Units, tests: Vec<MarkRow>, flaky: Vec<String>) -> Vec<(Check, String)> {
    let flaky: BTreeSet<String> = flaky.into_iter().collect();
    let mut out: Vec<(Check, String)> = tests
        .into_iter()
        .filter_map(|(suite, name, file, leg, quarantined)| {
            let test = format!("{suite} {name}").trim().to_owned();
            let why = if quarantined != 0 {
                "quarantined (W16-R05)"
            } else if flaky.contains(&test) {
                "flaky: it passed and failed on the same input (W16-R04)"
            } else {
                return None;
            };
            let job = leg.split('/').next().unwrap_or(&leg).to_owned();
            Some((Check { workflow: workflow.to_owned(), unit: unit_of(units, file.as_deref(), &suite, &job), test }, why.to_owned()))
        })
        .collect();
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// The longest `ci.verdict.wait` waits (W28-R58), as `deploy.releases.wait` does.
const WAIT_MAX_SECS: u64 = 600;
/// How many commits before a run's a refresh looks at again for a change (W28-R59).
const RECENT: usize = 50;

/// Whether every lane's verdict of a `ci.verdict` answer is final: answered (direct, covered or
/// culprit), not pending or suspect.
fn is_final(v: &Value) -> bool {
    let lanes = v.get("lanes").and_then(Value::as_array).cloned().unwrap_or_default();
    !lanes.is_empty() && lanes.iter().all(|l| matches!(l.pointer("/verdict/kind").and_then(Value::as_str), Some("direct" | "covered" | "culprit")))
}

/// `GET /api/ci/repos/:repo/commits/:sha/verdict/wait?until=&timeout_s=&workflow=`
/// (`ci.verdict.wait`, W28-R58): the commit's verdict once it is final (`until: final`, the
/// default) or once it changes at all (`until: any`), or when `timeout_s` (600 at most) runs out:
/// the verdict then, whether it is final, and whether the time ran out.
pub async fn wait(hub: &Hub, repo_key: &str, sha: &str, q: &HashMap<String, String>) -> Result<Value, Fail> {
    use futures::FutureExt as _;
    let any = q.get("until").map(String::as_str) == Some("any");
    let secs = q.get("timeout_s").and_then(|t| t.parse::<u64>().ok()).unwrap_or(WAIT_MAX_SECS).clamp(1, WAIT_MAX_SECS);
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut rx = hub.subscribe_events().map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("kiln's events can't be followed now: {e}")))?;
    let first = of_commit(hub, repo_key, sha, q).await?;
    let full = first.get("sha").and_then(Value::as_str).unwrap_or(sha).to_owned();
    let answer = |v: Value, timed_out: bool| json!({ "final": is_final(&v), "timed_out": timed_out, "verdict": v });
    if !any && is_final(&first) {
        return Ok(answer(first, false));
    }
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            let now = of_commit(hub, repo_key, sha, q).await?;
            return Ok(answer(now, true));
        }
        // A verdict of this commit changed, a run ended, or 30 s went by: look again. One timer
        // for the nap, not one per event heard: the hub's other events (an agent's output, every
        // few milliseconds) would put it off for as long as they kept coming.
        let nap = crate::rt::sleep(left.min(Duration::from_secs(30))).fuse();
        futures::pin_mut!(nap);
        let mut heard = false;
        while !heard {
            futures::select_biased! {
                () = hub.shutdown.cancelled().fuse() => return Ok(answer(first, true)),
                () = nap => break,
                m = rx.recv().fuse() => match m {
                    Ok(text) => {
                        // A verdict's and a run's events alone are parsed: the rest, every agent session's
                        // change of its info among them, are passed over by their front (goal 4, plan
                        // 01a1a59c2a9c step 5).
                        if !crate::wire::events::is_one_of(&text, &["ci_verdict", "run"]) {
                            continue;
                        }
                        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        heard = match v.get("type").and_then(Value::as_str) {
                            Some("ci_verdict") => v.get("sha").and_then(Value::as_str) == Some(full.as_str()),
                            Some("run") => ended(&v),
                            _ => false,
                        };
                    }
                    Err(async_broadcast::RecvError::Overflowed(_)) => heard = true,
                    Err(async_broadcast::RecvError::Closed) => break,
                },
            }
        }
        let now = of_commit(hub, repo_key, sha, q).await?;
        if (any && now != first) || (!any && is_final(&now)) {
            return Ok(answer(now, false));
        }
    }
}

/// Whether a `run` event says the run ended.
fn ended(v: &Value) -> bool {
    matches!(v.pointer("/run/status").and_then(Value::as_str), Some("succeeded" | "failed" | "cancelled" | "skipped" | "timed_out"))
}

/// Follows CI's runs for as long as the server runs: when one ends, its lane's recent verdicts
/// are worked out again (W28-R59).
pub async fn verdict_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    let mut rx = hub.subscribe_events()?;
    loop {
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            m = rx.recv().fuse() => match m {
                Ok(text) => {
                    // A run's events alone are parsed: the rest, every agent session's change of its info
                    // among them, are passed over by their front (goal 4, plan 01a1a59c2a9c step 5).
                    if !crate::wire::events::is_one_of(&text, &["run"]) {
                        continue;
                    }
                    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    if v.get("type").and_then(Value::as_str) != Some("run") {
                        continue;
                    }
                    let (Some(id), true) = (v.pointer("/run/id").and_then(Value::as_str).map(str::to_owned), ended(&v)) else { continue };
                    // A workflow's run (v2) that ended: its lane.
                    let lane: Option<(String, String, String, String)> = hub
                        .store
                        .call(move |c| {
                            use rusqlite::OptionalExtension as _;
                            Ok(c.query_row("SELECT repo_id, workflow_id, branch, sha FROM runs WHERE id = ?1 AND COALESCE(workflow_id, '') != ''", [&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?)
                        })
                        .await
                        .ok()
                        .flatten();
                    if let Some((repo, workflow, branch, sha)) = lane {
                        refresh(&hub, &repo, &workflow, &branch, &sha).await;
                    }
                }
                Err(async_broadcast::RecvError::Overflowed(_)) => {}
                Err(async_broadcast::RecvError::Closed) => return Ok(()),
            },
        }
    }
}

/// Works out the lane's verdicts again around `sha` (W28-R59): each commit whose verdict changed
/// since it was last kept gets a `ci_verdict` event, and the agent that pushed it a
/// `verdict.changed` message: a change of a verdict kept before, or the first answer of a commit
/// the run at `sha` newly answers (one after the lane's last run before it).
pub async fn refresh(hub: &Arc<Hub>, repo_key: &str, workflow: &str, branch: &str, sha: &str) {
    let Ok(repos) = hub.store.repos().await else { return };
    let Some(repo) = repos.into_iter().find(|r| r.name == repo_key || r.id == repo_key) else { return };
    let mirror = crate::gitops::mirror_path(hub, &repo.name).to_string_lossy().into_owned();
    let Ok(Some(l)) = lane_book(hub, (&repo.id, &repo.name), &mirror, workflow, branch, sha).await else { return };
    let l = Arc::new(l);
    refresh_with(hub, &repo, &mirror, (workflow, branch), sha, &l).await;
    // The book let go of on the blocking pool, off the shards (goal 9, plan 01a1a87b2cec step 3): it holds a result of
    // each test at each commit with a run, each freed one by one where it's dropped.
    let _ = crate::rt::blocking(move || drop(l)).await;
}

/// [`refresh`] with the lane's book: culprit findings started where declared, then each changed verdict told.
async fn refresh_with(hub: &Arc<Hub>, repo: &crate::store::Repo, mirror: &str, (workflow, branch): (&str, &str), sha: &str, l: &Arc<LaneBook>) {
    let Some(at) = l.shas.iter().position(|s| s == sha) else { return };
    // W28-R57: culprit finding for each new failure with two or more suspects, once declared.
    if hub.cfg.ci.culprits {
        let started = super::bisect::of_lane(hub, &repo.name, workflow, branch).await;
        // The failures to find culprits for, worked out on the compute pool, off the shards (goal 9, plan 01a1a87b2cec step
        // 3): new failures are looked for in every check's results.
        let book = Arc::clone(l);
        for (bad, check) in crate::rt::compute(move || to_bisect(&book, &started)).await.unwrap_or_default() {
            let (rid, w, br, s) = (repo.id.clone(), workflow.to_owned(), branch.to_owned(), bad);
            let run: Option<String> = hub
                .store
                .call(move |c| {
                    use rusqlite::OptionalExtension as _;
                    Ok(c.query_row("SELECT id FROM runs WHERE repo_id = ?1 AND workflow_id = ?2 AND branch = ?3 AND sha = ?4 ORDER BY created_at DESC LIMIT 1", rusqlite::params![rid, w, br, s], |r| r.get::<_, String>(0)).optional()?)
                })
                .await
                .ok()
                .flatten();
            if let Some(run) = run {
                super::bisect::start_auto(hub, &run, &check).await;
            }
        }
    }
    // The commits the run at `sha` newly answers: after the lane's last run before it.
    let after = l.runs.range(..at).next_back().map_or(0, |p| p + 1);
    // Each commit's verdict from RECENT before `sha` on, worked out on the compute pool a commit at a time, off the shards
    // (goal 9, plan 01a1a87b2cec step 3): its kind and why, which say whether it changed. A verdict holds each check's,
    // thousands at thousands of tests.
    let from = at.saturating_sub(RECENT);
    let book = Arc::clone(l);
    let Ok(kinds) = crate::rt::compute_map((from..l.shas.len()).collect::<Vec<usize>>(), move |c| kind_and_why(&book, c)).await else { return };
    for ((c, commit), now) in l.shas.iter().enumerate().skip(from).zip(kinds) {
        let Some((kind, why)) = now else { continue };
        let key = format!("ci/verdict/{}/{workflow}/{branch}/{commit}", repo.name);
        let kept = format!("{kind:?}: {why}");
        let before = hub.store.kv_get(key.clone()).await.ok().flatten();
        if before.as_deref() == Some(kept.as_str()) {
            continue;
        }
        if hub.store.kv_set(key, kept).await.is_err() {
            continue;
        }
        let news = match &before {
            Some(_) => true,
            None => (after..=at).contains(&c),
        };
        // The verdict as JSON, its event written out and its message where it's news, made on the compute pool too: a
        // verdict that changed is serialized whole, and its event, the verdict in it, was written again on this shard by
        // `Hub::emit` (goal 9, plan 01a1a87b2cec step 3).
        let (book, lane, sha) = (Arc::clone(l), (repo.name.clone(), workflow.to_owned(), branch.to_owned()), commit.clone());
        let Ok(Some((event, text))) = crate::rt::compute(move || {
            let (name, wf, br) = &lane;
            told(&book, c, news, (name, wf, br)).map(|(verdict, text)| (event_written(&lane, sha, kind, why, verdict), text))
        })
        .await
        else {
            continue;
        };
        hub.emit_written(event);
        // The push that brought it: the lane's first run at or after it (a culprit-finding try's
        // passed over).
        let pushed: Vec<&str> = l.runs.range(c..).take(8).filter_map(|p| l.shas.get(*p)).map(String::as_str).collect();
        if news && kind != Kind::Pending
            && let Some(text) = text
            && let Some(agent) = pusher(hub, repo, mirror, (workflow, branch), commit, &pushed).await
        {
            crate::workspace::follow::deliver(hub, &agent, text).await;
        }
    }
}

/// The lane's new failures with two or more suspects that none of `started`, its bisections, took on already (W28-R57):
/// each one's failing commit and check, in the book's order. On the thread it's called on, a compute job's.
fn to_bisect(l: &LaneBook, started: &[Bisect]) -> Vec<(String, Check)> {
    let mut out = Vec::new();
    for f in l.book.failures().into_iter().filter(|f| f.suspects.len() >= 2) {
        let Some(bad) = l.shas.get(f.commit).cloned() else { continue };
        let test = (!f.check.test.is_empty()).then(|| f.check.test.clone());
        if started.iter().any(|b| b.bad == bad && b.test == test && (test.is_some() || b.job == f.check.unit)) {
            continue;
        }
        out.push((bad, f.check));
    }
    out
}

/// Commit `sha`'s verdict event (`verdict`, its kind and why) on lane `lane` (repository, workflow, branch), written out
/// as [`crate::hub::Hub::emit`] writes it: on the thread it's called on, a compute job's.
fn event_written(lane: &(String, String, String), sha: String, kind: Kind, why: String, verdict: Value) -> Arc<str> {
    let (repo, workflow, branch) = lane.clone();
    Arc::from(crate::wire::events::ServerEvent::CiVerdict(Box::new(crate::wire::events::CiVerdict { repo, workflow, branch, sha, kind, why, verdict })).text())
}

/// Commit `c`'s verdict's kind and why, which say whether it changed: on the thread it's called on, a compute job's.
fn kind_and_why(l: &LaneBook, c: usize) -> Option<(Kind, String)> {
    l.book.verdict(c).map(|v| (v.kind, v.why))
}

/// Commit `c`'s verdict as an agent reads it, with its `verdict.changed` message when it's `news` and not pending, on the
/// lane of `repo`'s `workflow` on `branch`: on the thread it's called on, a compute job's.
fn told(l: &LaneBook, c: usize, news: bool, (repo, workflow, branch): (&str, &str, &str)) -> Option<(Value, Option<String>)> {
    let v = l.book.verdict(c)?;
    let sha = l.shas.get(c)?;
    let text = (news && v.kind != Kind::Pending).then(|| message(repo, workflow, branch, sha, &v, &l.shas));
    Some((l.named(&v), text))
}

/// The agent that pushed a commit, as step 6 attributes it (W28-R67, `kiln_ci::sched::actor`):
/// the agent token its push came with, the agent change request whose last pushed commit it is,
/// or its `Kiln-Agent` line. Its push is the one that made the lane's first run at or after it
/// (of `pushed`, the commits with runs from it on, the first whose run isn't a culprit-finding
/// try): what kiln kept of that push names the token (its event's `started_by`), which is the
/// agent's only for the commits that push brought, not those its branch already held.
async fn pusher(hub: &Hub, repo: &crate::store::Repo, mirror: &str, (workflow, branch): (&str, &str), sha: &str, pushed: &[&str]) -> Option<String> {
    use kiln_ci::sched::actor;
    let message = git(hub, &["-C", mirror, "log", "-1", "--format=%B", sha]).await.unwrap_or_default();
    let (rid, w, b, at) = (repo.id.clone(), workflow.to_owned(), branch.to_owned(), pushed.iter().map(|s| (*s).to_owned()).collect::<Vec<String>>());
    let rows: Vec<(String, String)> = hub
        .store
        .call(move |c| {
            use rusqlite::OptionalExtension as _;
            let mut out = Vec::new();
            for s in &at {
                let row = c
                    .query_row(
                        "SELECT r.actor, COALESCE(p.input, '') FROM runs r LEFT JOIN ci_plans p ON p.run_id = r.id WHERE r.repo_id = ?1 AND r.workflow_id = ?2 AND r.branch = ?3 AND r.sha = ?4 ORDER BY r.created_at LIMIT 1",
                        rusqlite::params![rid, w, b, s],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                    )
                    .optional()?;
                out.extend(row);
            }
            Ok(out)
        })
        .await
        .unwrap_or_default();
    let input = |text: &str| serde_json::from_str::<Value>(text).unwrap_or(Value::Null);
    let (actor_of_run, saved) = rows.into_iter().map(|(a, i)| (a, input(&i))).find(|(_, v)| v.get("culprit").is_none_or(Value::is_null)).unwrap_or_default();
    let event = saved.get("event").cloned().unwrap_or(Value::Null);
    let text = |p: &str| event.pointer(p).and_then(Value::as_str);
    let mut by = actor::pusher(text("/started_by/name"), text("/actor").unwrap_or(actor_of_run.as_str()));
    if by.starts_with("agent:") && !brought(hub, mirror, repo, branch, text("/before"), sha).await {
        by.clear();
    }
    let changes: Vec<crate::workspace::change::Record> = hub.store.kv_prefix("agents/change/".into()).await.unwrap_or_default().iter().filter_map(|(_, v)| serde_json::from_str(v).ok()).collect();
    let refs: Vec<actor::Change> = changes.iter().map(|c| actor::Change { agent: &c.agent, sha: &c.sha, change: c.change.as_deref() }).collect();
    let who = actor::attribute(&actor::Push { sha, pusher: &by, message: &message }, &refs);
    who.id.strip_prefix("agent:").filter(|id| !id.is_empty()).map(str::to_owned)
}

/// Whether a push to `branch` from `before` brought commit `sha`: it isn't one `before` held, or
/// for a push that made the branch, one the default branch held (W28-R67's token counts for those).
async fn brought(hub: &Hub, mirror: &str, repo: &crate::store::Repo, branch: &str, before: Option<&str>, sha: &str) -> bool {
    let base = match before.filter(|b| b.bytes().any(|c| c != b'0')) {
        Some(b) => Some(b.to_owned()),
        None => Some(repo.default_branch.clone().unwrap_or_else(|| "main".into())).filter(|d| d != branch).map(|d| format!("refs/heads/{d}")),
    };
    match base {
        Some(base) => crate::gitops::run_git(hub, &["-C", mirror, "merge-base", "--is-ancestor", sha, &base], crate::gitops::LS_REMOTE_TIMEOUT).await.is_err(),
        None => true,
    }
}

/// A `verdict.changed` message (W28-R59): direct and covered in a line; suspect and culprit with
/// the failing check, its base, the commit that failed and how to look.
fn message(repo: &str, workflow: &str, branch: &str, sha: &str, v: &Verdict, shas: &[String]) -> String {
    let short = |s: &str| s.get(..12).unwrap_or(s).to_owned();
    let at = |p: Option<usize>| p.and_then(|p| shas.get(p)).map_or_else(String::new, |s| short(s));
    let look = format!("ci.verdict {{\"repo\": \"{repo}\", \"sha\": \"{sha}\"}} shows each check.");
    let head = format!("verdict.changed: your commit {} on {branch} ({workflow})", short(sha));
    let bad = v.checks.iter().find(|c| matches!(c.status, kiln_ci::verdict::Status::Suspect | kiln_ci::verdict::Status::Culprit));
    match (v.kind, bad) {
        (Kind::Suspect, Some(c)) => format!(
            "{head} is a suspect: {} newly failed at {} (it passed at {}), and your diff could affect {}. A culprit finding clears or names it: ci.runs.bisect {{\"run\": <that run>, \"test\": \"{}\"}}. {look}",
            c.check, at(c.by), at(c.base), c.check.unit, c.check.test
        ),
        (Kind::Culprit, Some(c)) => format!("{head} is the culprit: {}. Fix it and push again. {look}", c.why),
        (Kind::Covered, _) => format!("{head} is covered (optimistic, not passed): {}. {look}", v.why),
        _ => format!("{head} is {}: {}. {look}", serde_json::to_value(v.kind).ok().and_then(|k| k.as_str().map(str::to_owned)).unwrap_or_default(), v.why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::bisect::Culprit;

    /// The workspace's root manifest at the lane's newest commit: its members are `crates/*`.
    const ROOT: &str = "[workspace]\nmembers = [\"crates/*\"]\n";
    /// The tree's files at that commit, as `git ls-tree -r --name-only` lists them.
    const LISTED: &str = "Cargo.lock\nCargo.toml\ncrates/app/Cargo.toml\ncrates/app/src/main.rs\ncrates/core/Cargo.toml\ncrates/core/src/lib.rs\ndocs/ci.md\n";
    /// W16's flaky mark, as [`marks`] words it.
    const FLAKY: &str = "flaky: it passed and failed on the same input (W16-R04)";
    /// W16's quarantine mark, as [`marks`] words it.
    const QUARANTINED: &str = "quarantined (W16-R05)";

    /// A member's manifest at that commit, as `git show` reads it: `app` depends on `my-core` by path.
    fn manifest(path: &str) -> Option<String> {
        match path {
            "crates/app/Cargo.toml" => Some("[package]\nname = \"app\"\n[dependencies]\nmy-core = { path = \"../core\" }\n".to_owned()),
            "crates/core/Cargo.toml" => Some("[package]\nname = \"my-core\"\n".to_owned()),
            _ => None,
        }
    }

    /// The workspace's units, `app` and `my-core`, made as `units_at` made them on its shard.
    fn units() -> Units {
        let files: Vec<String> = LISTED.lines().map(str::to_owned).collect();
        Units::new(kiln_ci::verdict::cargo::units(ROOT, &files, manifest).unwrap_or_default())
    }

    /// The lane's commit at place `i`.
    fn sha(i: usize) -> String {
        i.to_string().repeat(40)
    }

    fn test(unit: &str, name: &str) -> Check {
        Check { workflow: "ci".into(), unit: unit.into(), test: name.into() }
    }

    fn ran(commit: usize, check: &Check, outcome: Outcome, at: i64) -> Seen {
        Seen { commit, check: check.clone(), outcome, how: How::Ran, at }
    }

    fn failed(signature: &str) -> Outcome {
        Outcome::Failed { signature: signature.into() }
    }

    /// A lane of five commits and what `lane_book` reads for its book: each commit's diff (c1 changes my-core, and so app;
    /// c2 app; c3 both; c4 a file no unit takes), three runs' results and W16's marks (at c0, c3 and c4: app's test newly
    /// fails at c3, my-core's is marked flaky, then quarantined), and five bisections, two of which the book doesn't take.
    fn scenario() -> (Vec<String>, Vec<Option<String>>, Vec<Said>, Vec<Bisect>) {
        let (t_core, t_app, lint) = (test("my-core", "my_core::tests it"), test("app", "app::tests main"), Check::unit("ci", "lint"));
        let shas: Vec<String> = (0..5).map(sha).collect();
        let diffs = vec![
            None,
            Some("crates/core/src/lib.rs\n".to_owned()),
            Some("crates/app/src/main.rs\n".to_owned()),
            Some("crates/core/src/lib.rs\n\ncrates/app/src/main.rs\n".to_owned()),
            Some("docs/ci.md\n".to_owned()),
        ];
        let said = vec![
            (vec![ran(0, &t_core, Outcome::Passed, 10), ran(0, &t_app, Outcome::Passed, 10), ran(0, &lint, Outcome::Passed, 11)], Vec::new()),
            (vec![ran(3, &t_core, Outcome::Passed, 30), ran(3, &t_app, failed(&t_app.test), 30), ran(3, &lint, failed("failure"), 31)], vec![(t_core.clone(), FLAKY.to_owned())]),
            (vec![ran(4, &t_core, failed(&t_core.test), 40), ran(4, &t_app, failed(&t_app.test), 40), ran(4, &lint, Outcome::Skipped, 41)], vec![(t_core.clone(), QUARANTINED.to_owned())]),
        ];
        let bisections = vec![
            Bisect { state: "found".into(), culprit: Some(Culprit { sha: sha(2), ..Culprit::default() }), test: Some(t_app.test.clone()), unit: Some("app".into()), job: "test".into(), suspects: vec![sha(1), sha(2), sha(3)], updated_at: 50, ..Bisect::default() },
            Bisect { state: "no-culprit".into(), job: "lint".into(), commits: vec![sha(3)], updated_at: 51, ..Bisect::default() },
            Bisect { state: "found".into(), culprit: Some(Culprit { sha: "f".repeat(40), ..Culprit::default() }), job: "test".into(), commits: vec![sha(4)], updated_at: 52, ..Bisect::default() },
            Bisect { state: "running".into(), job: "test".into(), commits: vec![sha(4)], updated_at: 53, ..Bisect::default() },
            Bisect { state: "inconclusive".into(), why: Some("its tries disagreed".into()), test: Some(t_core.test.clone()), job: "test".into(), suspects: vec![sha(4), "e".repeat(40)], updated_at: 54, ..Bisect::default() },
        ];
        (shas, diffs, said, bisections)
    }

    /// The book as `lane_book` made it on its shard before goal 9's step 3: each commit pushed as its diff was read, each
    /// run's results seen and its marks marked as they were read, then the findings.
    fn book_inline(units: &Units, shas: &[String], diffs: &[Option<String>], said: &[Said], bisections: &[Bisect]) -> Book {
        let mut book = Book::default();
        for (i, (s, diff)) in shas.iter().zip(diffs).enumerate() {
            let affects = if i == 0 {
                Affects::All
            } else if let Some(changed) = diff {
                let files: Vec<&str> = changed.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
                if files.iter().any(|f| units.takes(f).is_empty()) { Affects::All } else { Affects::Units(units.affected(files)) }
            } else {
                Affects::All
            };
            book.push(Commit { id: s.clone(), at: 0, affects });
        }
        for (seen, marked) in said {
            for s in seen {
                book.see(s.clone());
            }
            for (check, why) in marked {
                book.mark(check.clone(), why.clone());
            }
        }
        let place: BTreeMap<&str, usize> = shas.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();
        for b in bisections {
            let suspects: Vec<usize> = b.suspects().iter().filter_map(|s| place.get(s.as_str()).copied()).collect();
            let finding = match (b.state.as_str(), &b.culprit) {
                ("found", Some(c)) => match place.get(c.sha.as_str()) {
                    Some(&commit) => Finding::Culprit { commit },
                    None => continue,
                },
                ("no-culprit", _) => Finding::NoCulprit,
                ("inconclusive", _) => Finding::Inconclusive { why: b.why.clone().unwrap_or_default() },
                _ => continue,
            };
            let check = match &b.test {
                Some(t) => Check { workflow: "ci".to_owned(), unit: b.unit.clone().unwrap_or_else(|| b.job.clone()), test: t.clone() },
                None => Check::unit("ci", &b.job),
            };
            book.found(Found { check, suspects, finding, at: b.updated_at });
        }
        book
    }

    /// Goal 9, plan 01a1a87b2cec step 3: what `units_at` makes of what git gave it, now on the compute pool (the tree's
    /// files and the members' manifests among them, then the manifests parsed into units), is what its shard made.
    #[test]
    fn g9_s3_the_workspaces_units_made_off_the_shard_are_the_inline_ones() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            // As the shard made them: each listed file a String, the members' manifests read, the units.
            let files: Vec<String> = LISTED.lines().map(str::to_owned).collect();
            let mut read: BTreeMap<String, String> = BTreeMap::new();
            for f in files.iter().filter(|f| f.ends_with("Cargo.toml") && f.as_str() != "Cargo.toml") {
                if let Some(t) = manifest(f) {
                    read.insert(f.clone(), t);
                }
            }
            let inline = Units::new(kiln_ci::verdict::cargo::units(ROOT, &files, |p| read.get(p).cloned()).unwrap_or_default());
            // As they're made now.
            let (listed, members) = crate::rt::compute(|| listing(LISTED)).await?;
            anyhow::ensure!(listed == files && members == ["crates/app/Cargo.toml", "crates/core/Cargo.toml"], "{listed:?} {members:?}");
            let manifests: BTreeMap<String, String> = members.into_iter().filter_map(|f| manifest(&f).map(|t| (f, t))).collect();
            anyhow::ensure!(manifests == read, "the same manifests read: {manifests:?}");
            let got = crate::rt::compute(move || units_of(ROOT, &listed, &manifests)).await?;
            anyhow::ensure!(got == inline, "{got:?} != {inline:?}");
            let affected: BTreeSet<String> = ["app", "my-core"].map(str::to_owned).into();
            anyhow::ensure!(got.affected(["crates/core/src/lib.rs"]) == affected, "a change to my-core affects app, which depends on it");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a run's rows, made its results and W16's marks on the compute pool, are what the
    /// shard made of them: each test's unit (its file's, its suite's crate, else its job), name and outcome, each job
    /// without reports, and the marks sorted, one a check.
    #[test]
    fn g9_s3_a_runs_results_and_marks_made_off_the_shard_are_the_inline_ones() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let units = Arc::new(units());
            let tests: Vec<TestRow> = vec![
                ("my_core::tests".into(), "it".into(), Some("crates/core/src/lib.rs".into()), "test/1".into(), "passed".into(), 10),
                ("app_tests".into(), "main".into(), None, "test/2".into(), "failed".into(), 11),
                ("my_core::tests".into(), "other".into(), None, "test/1".into(), "error".into(), 12),
                ("docs".into(), "links".into(), Some("docs/ci.md".into()), "docs/1".into(), "skipped".into(), 13),
            ];
            let jobs: Vec<JobRow> = vec![
                ("test".into(), Some("failure".into()), 20),
                ("lint/2".into(), Some("success".into()), 21),
                ("build".into(), Some("timed_out".into()), 22),
                ("fmt".into(), Some("skipped".into()), 23),
                ("deploy".into(), Some("cancelled".into()), 24),
                ("docs".into(), Some("success".into()), 25),
            ];
            let inline = seen_of("ci", &units, 3, tests.clone(), jobs.clone());
            let u = Arc::clone(&units);
            let got = crate::rt::compute(move || seen_of("ci", &u, 3, tests, jobs)).await?;
            anyhow::ensure!(got == inline, "{got:?} != {inline:?}");
            let want = [
                ran(3, &test("my-core", "my_core::tests it"), Outcome::Passed, 10),
                ran(3, &test("test", "app_tests main"), failed("app_tests main"), 11),
                ran(3, &test("my-core", "my_core::tests other"), failed("my_core::tests other"), 12),
                ran(3, &Check::unit("ci", "lint"), Outcome::Passed, 21),
                ran(3, &Check::unit("ci", "build"), failed("timed_out"), 22),
                ran(3, &Check::unit("ci", "fmt"), Outcome::Skipped, 23),
            ];
            anyhow::ensure!(got == want, "{got:?}");
            let rows: Vec<MarkRow> = vec![
                ("my_core::tests".into(), "it".into(), Some("crates/core/src/lib.rs".into()), "test/1".into(), 0),
                ("app_tests".into(), "main".into(), None, "test/2".into(), 1),
                ("my_core::tests".into(), "other".into(), None, "test/1".into(), 0),
                ("my_core::tests".into(), "it".into(), None, "test/3".into(), 1),
            ];
            let flaky = vec!["my_core::tests it".to_owned()];
            let inline = marks_of("ci", &units, rows.clone(), flaky.clone());
            let got = crate::rt::compute(move || marks_of("ci", &units, rows, flaky)).await?;
            anyhow::ensure!(got == inline, "{got:?} != {inline:?}");
            anyhow::ensure!(got == [(test("my-core", "my_core::tests it"), FLAKY.to_owned()), (test("test", "app_tests main"), QUARANTINED.to_owned())], "{got:?}");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: the lane's book, now made on the compute pool in pieces from what `lane_book`
    /// read, is the book its shard made as the reads came: the same commits (what each diff could affect), verdicts and
    /// new failures, W16's last mark on a check the one that counts and the findings at their places. The runs it reads
    /// results for are picked out of the lane's as the shard picked them.
    #[test]
    fn g9_s3_the_lane_book_made_in_pieces_off_the_shard_is_the_inline_one() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let units = Arc::new(units());
            let (shas, diffs, said, bisections) = scenario();
            let inline = book_inline(&units, &shas, &diffs, &said, &bisections);
            let (got, book) = made(Making::new(Arc::clone(&units), "ci", shas.clone(), diffs, said, bisections)).await?;
            anyhow::ensure!(got == shas, "the commits come back: {got:?}");
            anyhow::ensure!(book.commits() == inline.commits(), "{:?} != {:?}", book.commits(), inline.commits());
            anyhow::ensure!(book.verdicts() == inline.verdicts(), "{:?} != {:?}", book.verdicts(), inline.verdicts());
            anyhow::ensure!(book.failures() == inline.failures(), "{:?} != {:?}", book.failures(), inline.failures());
            // The culprit found is named at c2; my-core's test failing at c4 doesn't count, as run 4 quarantined it after
            // run 3 marked it flaky.
            anyhow::ensure!(book.verdict(2).map(|v| v.kind) == Some(Kind::Culprit), "{:?}", book.verdict(2));
            let quarantined = book.verdict(4).is_some_and(|v| v.checks.iter().any(|c| c.check.unit == "my-core" && c.why.contains("marks it quarantined (W16-R05)")));
            anyhow::ensure!(quarantined, "{:?}", book.verdict(4));
            // The runs at the lane's commits, and its places with runs, as the shard picked them out.
            let runs = vec![
                RunRow { id: "r0".into(), sha: sha(0), attempt: 1 },
                RunRow { id: "gone".into(), sha: "a".repeat(40), attempt: 1 },
                RunRow { id: "r3".into(), sha: sha(3), attempt: 2 },
                RunRow { id: "r4".into(), sha: sha(4), attempt: 1 },
            ];
            let place: BTreeMap<&str, usize> = shas.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();
            let mut inline_at = Vec::new();
            for run in runs.iter().filter(|r| place.contains_key(r.sha.as_str())) {
                let Some(&p) = place.get(run.sha.as_str()) else { continue };
                inline_at.push((p, run.id.clone(), run.attempt));
            }
            let inline_with: BTreeSet<usize> = runs.iter().filter_map(|r| place.get(r.sha.as_str()).copied()).collect();
            let commits = shas.clone();
            let (at, with) = crate::rt::compute(move || runs_at(&commits, runs)).await?;
            let at: Vec<(usize, String, i64)> = at.into_iter().map(|(p, r)| (p, r.id, r.attempt)).collect();
            anyhow::ensure!(at == inline_at && with == inline_with, "{at:?} {with:?}");
            anyhow::ensure!(at == [(0, "r0".to_owned(), 1), (3, "r3".to_owned(), 2), (4, "r4".to_owned(), 1)], "{at:?}");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: what `refresh` now works out on the compute pool is what its shard worked out:
    /// each commit's verdict's kind and why (a commit a unit), a changed verdict as JSON with its `verdict.changed`
    /// message, and the new failures to find culprits for.
    #[test]
    fn g9_s3_refreshs_verdicts_messages_and_culprit_findings_off_the_shard_are_the_inline_ones() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (shas, diffs, said, bisections) = scenario();
            let (shas, book) = made(Making::new(Arc::new(units()), "ci", shas, diffs, said, bisections)).await?;
            let l = Arc::new(LaneBook { shas, runs: [0, 3, 4].into(), book });
            let n = l.shas.len();
            let book = Arc::clone(&l);
            let kinds = crate::rt::compute_map((0..n).collect::<Vec<usize>>(), move |c| kind_and_why(&book, c)).await?;
            let inline: Vec<Option<(Kind, String)>> = (0..n).map(|c| l.book.verdict(c).map(|v| (v.kind, v.why))).collect();
            anyhow::ensure!(kinds == inline, "{kinds:?} != {inline:?}");
            for c in 0..n {
                for news in [false, true] {
                    let book = Arc::clone(&l);
                    let got = crate::rt::compute(move || told(&book, c, news, ("kiln", "ci", "main"))).await?;
                    let want = l.book.verdict(c).map(|v| (l.named(&v), (news && v.kind != Kind::Pending).then(|| message("kiln", "ci", "main", &sha(c), &v, &l.shas))));
                    anyhow::ensure!(got == want, "commit {c}, news {news}: {got:?} != {want:?}");
                }
            }
            // app's test newly failed at c3, with three suspects; lint's new failure there has none, as no diff could
            // affect it.
            let book = Arc::clone(&l);
            let to_find = crate::rt::compute(move || to_bisect(&book, &[])).await?;
            anyhow::ensure!(to_find == [(sha(3), test("app", "app::tests main"))], "{to_find:?}");
            // One a bisection took on already is left out.
            let started = [Bisect { bad: sha(3), test: Some("app::tests main".into()), job: "test".into(), ..Bisect::default() }];
            let book = Arc::clone(&l);
            let to_find = crate::rt::compute(move || to_bisect(&book, &started)).await?;
            anyhow::ensure!(to_find.is_empty(), "{to_find:?}");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a changed verdict's event is written out on the compute pool, as `Hub::emit`
    /// wrote it on its caller's shard, and goes to the hub's listeners as it would have, and to the events sockets'
    /// feed (W21-R15) as `Hub::emit`'s goes: `refresh` writes none on its shard.
    #[test]
    fn g9_s3_a_verdicts_event_is_written_off_the_shard_as_emit_wrote_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let mut events = hub.subscribe_events()?;
            let verdict = json!({"commit": sha(1), "kind": "culprit", "checks": [{"check": {"workflow": "ci", "unit": "app", "test": "app::tests main"}, "why": "it fails here first"}]});
            let event = crate::wire::events::ServerEvent::CiVerdict(Box::new(crate::wire::events::CiVerdict { repo: "kiln".into(), workflow: "ci".into(), branch: "main".into(), sha: sha(1), kind: Kind::Culprit, why: "app::tests main fails here first".into(), verdict: verdict.clone() }));
            let lane = ("kiln".to_owned(), "ci".to_owned(), "main".to_owned());
            let written = crate::rt::compute(move || event_written(&lane, sha(1), Kind::Culprit, "app::tests main fails here first".into(), verdict)).await?;
            anyhow::ensure!(*written == *event.text(), "{written}");
            let from = hub.feed.head();
            hub.emit_written(Arc::clone(&written));
            hub.emit(&event);
            let off = crate::rt::timeout(std::time::Duration::from_secs(5), events.recv()).await??;
            let on = crate::rt::timeout(std::time::Duration::from_secs(5), events.recv()).await??;
            anyhow::ensure!(off == written && on == written, "the same to its listeners: {off} / {on}");
            // The events sockets read the feed alone: both numbered there, the same text.
            let fed = match hub.feed.since(from) {
                crate::wire::events::feed::Since::Frames(frames) => frames.iter().filter(|f| *f.text == *written).count(),
                crate::wire::events::feed::Since::Gap => 0,
            };
            anyhow::ensure!(fed == 2, "the same to the events sockets' feed: {fed} of 2");
            let code = include_str!("verdict.rs").split("\n#[cfg(test)]\nmod tests {").next().unwrap_or_default().lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n");
            let body = code.split("async fn refresh_with(").nth(1).and_then(|rest| rest.split("\n}\n").next()).unwrap_or_default();
            anyhow::ensure!(!body.is_empty() && body.contains("hub.emit_written(") && !body.contains("hub.emit(&"), "refresh writes its verdicts' events off its shard");
            Ok(())
        })
    }
}
