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

/// A lane's runs, oldest first: from `runs_lane` alone, which holds their commits and attempts and
/// gives them in this order (the DB query audit, 2026-11-06: 3.3 -> 0.9 ms for a lane of 3,400 runs).
/// Runs made in one second by number: a check marked in two runs keeps the later run's word.
const LANE_RUNS: &str = "SELECT id, sha, attempt FROM runs WHERE repo_id = ?1 AND workflow_id = ?2 AND branch = ?3 ORDER BY created_at, number";
/// A commit's latest run on a lane, which a culprit finding starts from, and its first, whose actor
/// pushed it, with the plan's input: among the commit's runs (a few), not the lane's walked to it (the
/// DB query audit, 2026-11-06: 1.2-3.0 ms -> 15-62 us on a lane of 3,400 runs). Of runs made in one
/// second, the latest is the greater number and the first the smaller.
const CULPRIT_RUN: &str = "SELECT id FROM runs INDEXED BY runs_repo_sha WHERE repo_id = ?1 AND workflow_id = ?2 AND branch = ?3 AND sha = ?4 ORDER BY created_at DESC, number DESC LIMIT 1";
const PUSHER_RUN: &str = "SELECT r.actor, COALESCE(p.input, '') FROM runs r INDEXED BY runs_repo_sha LEFT JOIN ci_plans p ON p.run_id = r.id WHERE r.repo_id = ?1 AND r.workflow_id = ?2 AND r.branch = ?3 AND r.sha = ?4 ORDER BY r.created_at, r.number LIMIT 1";
/// Every test result of the runs named (`?1`, JSON), each with its case if it's there: each run's in
/// the order they were kept.
const RAN_TESTS: &str = "SELECT t.run_id, t.repo_id, t.attempt, k.suite, k.name, k.file, t.leg_key, t.outcome, COALESCE(t.ts, 0), t.quarantined, t.retried
     FROM json_each(?1) r JOIN test_results t ON t.run_id = r.value LEFT JOIN test_cases k ON k.repo_id = t.repo_id AND k.id = t.case_id";
/// The v2 jobs of the runs named (`?1`, JSON: each run and its attempt) of those attempts.
const RAN_JOBS: &str = "SELECT json_extract(r.value, '$[0]'), j.job_id, j.conclusion, COALESCE(j.finished_at, 0)
     FROM json_each(?1) r JOIN jobs j ON j.run_id = json_extract(r.value, '$[0]') AND j.run_attempt = json_extract(r.value, '$[1]') WHERE j.job_id IS NOT NULL";
/// How many results each of the runs named (`?1`, JSON) has, and its last one's row: what says its
/// results changed, as a run's results are only ever added to (W16: a leg's report, a reused leg's).
const RAN_COUNTS: &str = "SELECT t.run_id, COUNT(*), MAX(t.id) FROM json_each(?1) r JOIN test_results t ON t.run_id = r.value GROUP BY t.run_id";
/// How many lanes' books are kept ([`Books`]): those read last.
const KEPT_LANES: usize = 16;
/// The most commits [`diffs_of`] names to one `git log`: its command line stays within Windows'
/// (32,767 characters), 41 a commit.
const AT_ONCE: usize = 256;

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
#[derive(Clone)]
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
/// parents hold `sha`: the book kept of it ([`Books`]) brought up to date, where what it was made of
/// moved on, else made again (the DB query audit, 2026-11-06).
async fn lane_book(hub: &Hub, (repo_id, repo): (&str, &str), mirror: &str, workflow: &str, branch: &str, sha: &str) -> Result<Option<Arc<LaneBook>>, Fail> {
    // The lane's commits: first parents up to the branch's tip, back to the commit and BACK more.
    let (r, w, b) = (repo_id.to_owned(), workflow.to_owned(), branch.to_owned());
    // Beside the writer, which a long lane's runs held up.
    let runs: Vec<RunRow> = hub
        .store
        .read(move |c| {
            let mut st = c.prepare_cached(LANE_RUNS)?;
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
    let key = (repo_id.to_owned(), workflow.to_owned(), branch.to_owned());
    let kept = hub.ci.books.take(&key);
    // The workspace's units, as the newest commit has them: the kept book's, where the manifests are the same.
    let (units_key, units) = units_at(hub, mirror, &head, kept.as_ref().map(|k| (k.units_key.as_str(), &k.units))).await;
    // The lane's runs at its commits, with their places, picked out on the compute pool, off the shards (goal 9, plan
    // 01a1a87b2cec step 3): the runs read are every one the lane ever had, thousands on a busy branch.
    let (shas, at_commits, with_runs) = crate::rt::compute(move || {
        let (at_commits, with_runs) = runs_at(&shas, runs);
        (shas, at_commits, with_runs)
    })
    .await
    .map_err(fail)?;
    // Each one's results as they stand (how many, and the last), and its attempt's jobs: what says whether it changed
    // since the book was kept.
    let asked: Vec<(String, i64)> = at_commits.iter().map(|(_, r)| (r.id.clone(), r.attempt)).collect();
    let (counts, mut jobs) = hub.store.read(move |c| Ok((counts_of(c, &asked)?, jobs_of(c, &asked)?))).await.map_err(fail)?;
    let mut now: Vec<KeptRun> = at_commits
        .into_iter()
        .map(|(place, r)| KeptRun { results: counts.get(&r.id).copied().unwrap_or_default(), jobs: jobs.remove(&r.id).unwrap_or_default(), id: r.id, attempt: r.attempt, place, marks: Vec::new() })
        .collect();
    // W28-R57: the lane's culprit findings, its bisections that ended.
    let bisections = super::bisect::of_lane(hub, repo, workflow, branch).await;
    let plan = kept.as_ref().filter(|k| k.units_key == units_key).and_then(|k| Plan::of(k, &shas, &now));
    let lane = match (kept, plan) {
        (Some(kept), Some(plan)) => {
            // Only the commits it gained are read (their diffs), and the runs at the places that changed.
            let diffs = diffs_of(hub, mirror, &plan.gained).await;
            let reread: Vec<&KeptRun> = now.iter().filter(|r| plan.places.contains(&r.place)).collect();
            let said = said_of(hub, repo_id, workflow, &units, &reread).await?;
            let read = Read { shas, diffs, said, bisections };
            let (u, w) = (Arc::clone(&units), workflow.to_owned());
            // Brought up to date on the compute pool, off the shards: letting go of the commits the lane left moves each
            // result it keeps.
            let (lane, runs) = crate::rt::compute(move || {
                let mut runs = now;
                let lane = kept_up(kept, &plan, read, with_runs, &mut runs, &u, &w);
                (lane, runs)
            })
            .await
            .map_err(fail)?;
            now = runs;
            Arc::new(lane)
        }
        (kept, _) => {
            // The one kept, if any, let go of on the blocking pool, off the shards: each of its results freed one by one.
            if let Some(k) = kept {
                let _ = crate::rt::blocking(move || drop(k)).await;
            }
            // Each commit's diff against its first parent, read in the lane's order; the first's isn't read, as it
            // affects every unit. What each diff could affect is worked out as the book is made.
            let diffs: Vec<Option<String>> = std::iter::once(None).chain(diffs_of(hub, mirror, shas.get(1..).unwrap_or_default()).await).collect();
            let all: Vec<&KeptRun> = now.iter().collect();
            let said = said_of(hub, repo_id, workflow, &units, &all).await?;
            // The book made of it all on the compute pool, in pieces, off the shards (goal 9, plan 01a1a87b2cec step 3):
            // each result it takes costs a microsecond or two, and a run has one for each of its tests, thousands of them.
            let making = anew(Read { shas, diffs, said, bisections }, &mut now, Arc::clone(&units), workflow);
            let (shas, book) = made(making).await.map_err(fail)?;
            Arc::new(LaneBook { shas, runs: with_runs, book })
        }
    };
    let gone = hub.ci.books.keep(key, Kept { lane: Arc::clone(&lane), units, units_key, runs: now });
    if !gone.is_empty() {
        let _ = crate::rt::blocking(move || drop(gone)).await;
    }
    Ok(Some(lane))
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

/// The books kept of the lanes read last (W28-R55; the DB query audit, 2026-11-06): each lane's, by repository, workflow
/// and branch, for its next read to bring up to date rather than make again. [`KEPT_LANES`] of them, the one read
/// longest ago let go of first; one being brought up to date is out of it meanwhile (a read of the lane alongside makes
/// its own).
#[derive(Default)]
pub struct Books(parking_lot::Mutex<Vec<((String, String, String), Kept)>>);

impl Books {
    /// The lane's kept book, taken out to be brought up to date.
    fn take(&self, lane: &(String, String, String)) -> Option<Kept> {
        let mut kept = self.0.lock();
        let i = kept.iter().position(|(k, _)| k == lane)?;
        Some(kept.remove(i).1)
    }

    /// Keeps the lane's book; those it puts past [`KEPT_LANES`], for the caller to let go of off the shards.
    fn keep(&self, lane: (String, String, String), book: Kept) -> Vec<Kept> {
        let mut kept = self.0.lock();
        let mut gone: Vec<Kept> = Vec::new();
        if let Some(i) = kept.iter().position(|(k, _)| *k == lane) {
            gone.push(kept.remove(i).1);
        }
        kept.insert(0, (lane, book));
        let keep = KEPT_LANES.min(kept.len());
        gone.extend(kept.drain(keep..).map(|(_, k)| k));
        gone
    }
}

/// A lane's book as kept between reads, with what it was made of: the lane (its commits, those with runs, the book), the
/// workspace's units and what they were made of ([`units_at`]'s key), and the lane's runs at its commits.
pub(crate) struct Kept {
    lane: Arc<LaneBook>,
    units: Arc<Units>,
    units_key: String,
    runs: Vec<KeptRun>,
}

/// A run at a lane's commit, as the kept book took it: its place, what says whether it changed (its attempt, its
/// results' count and last row, its attempt's jobs) and W16's marks it gave.
#[derive(Debug, Clone)]
struct KeptRun {
    id: String,
    attempt: i64,
    place: usize,
    results: (i64, i64),
    jobs: Vec<JobRow>,
    marks: Vec<(Check, String)>,
}

impl KeptRun {
    /// Whether it's the run `other` is, as it was: its results and jobs the same.
    fn same(&self, other: &KeptRun) -> bool {
        self.id == other.id && self.attempt == other.attempt && self.results == other.results && self.jobs == other.jobs
    }
}

/// What changed in a lane since its book was kept, for [`up`] to bring the book up to date with: the commits it let go
/// of at its start (`drop`; read from a later commit, it starts later), those it gained at its end, and the places
/// whose runs changed (one made, gone, run again, with more results or another job's end there), whose results are
/// taken again.
#[derive(Debug, PartialEq)]
struct Plan {
    drop: usize,
    gained: Vec<String>,
    places: BTreeSet<usize>,
}

impl Plan {
    /// What changed from `kept` to the lane's commits `shas` and its runs at them `now`: none where its commits aren't
    /// the kept ones moved on (its branch moved back or elsewhere, or it starts earlier: an older commit asked about),
    /// which the book is made again for.
    fn of(kept: &Kept, shas: &[String], now: &[KeptRun]) -> Option<Plan> {
        let was = &kept.lane.shas;
        let drop = was.iter().position(|s| Some(s) == shas.first())?;
        let still = was.get(drop..)?;
        if shas.get(..still.len())? != still {
            return None;
        }
        let gained = shas.get(still.len()..)?.to_vec();
        // The runs at each place, as they were (moved with the commits) and as they are, in the lane's order.
        let mut before: BTreeMap<usize, Vec<&KeptRun>> = BTreeMap::new();
        for r in &kept.runs {
            if let Some(p) = r.place.checked_sub(drop) {
                before.entry(p).or_default().push(r);
            }
        }
        let mut after: BTreeMap<usize, Vec<&KeptRun>> = BTreeMap::new();
        for r in now {
            after.entry(r.place).or_default().push(r);
        }
        let same = |a: Option<&Vec<&KeptRun>>, b: Option<&Vec<&KeptRun>>| {
            let (a, b) = (a.map(Vec::as_slice).unwrap_or_default(), b.map(Vec::as_slice).unwrap_or_default());
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.same(y))
        };
        let places = before.keys().chain(after.keys()).copied().filter(|p| !same(before.get(p), after.get(p))).collect();
        Some(Plan { drop, gained, places })
    }
}

/// `book`, kept, brought up to date by `plan`: the commits the lane left let go of, those it gained pushed (each with what
/// its diff could affect, `affects`), every result at a place whose runs changed taken again from those runs (`said`,
/// by run; `runs`, the lane's runs at its commits in its order), W16's marks given again (of each check the last of the
/// runs' that marks it, as a book made of them in their order keeps), and the culprit findings (`found`, in their
/// order). The book a book made again of all of it would be. On the thread it's called on, a compute job's.
fn up(book: &mut Book, plan: &Plan, affects: Vec<Affects>, mut said: HashMap<String, Said>, runs: &[KeptRun], found: Vec<Found>) {
    book.clear_found();
    book.drop_front(plan.drop);
    // A lane's first commit affects every unit: its diff isn't read.
    if plan.drop > 0 {
        book.set_affects(0, Affects::All);
    }
    for (id, affects) in plan.gained.iter().zip(affects) {
        book.push(Commit { id: id.clone(), at: 0, affects });
    }
    for &p in &plan.places {
        book.clear(p);
        for r in runs.iter().filter(|r| r.place == p) {
            for seen in said.remove(&r.id).map(|(seen, _)| seen).unwrap_or_default() {
                book.see(seen);
            }
        }
    }
    let mut marks: BTreeMap<Check, String> = BTreeMap::new();
    for (check, why) in runs.iter().flat_map(|r| r.marks.iter()) {
        marks.insert(check.clone(), why.clone());
    }
    book.set_marks(marks);
    for f in found {
        book.found(f);
    }
}

/// What a read of a lane gave its book, beside its runs: its commits, the diffs read (of the commits gained, or of every
/// one), the runs' results and W16's marks read (by run), and its bisections.
struct Read {
    shas: Vec<String>,
    diffs: Vec<Option<String>>,
    said: HashMap<String, Said>,
    bisections: Vec<Bisect>,
}

/// The kept book brought up to date by `plan` ([`up`]) with what was `read`: the lane's commits and those with runs now
/// (`with_runs`), its runs at its commits (`runs`, in its order) given W16's marks, those read again's as read and the
/// rest's as kept. On the thread it's called on, a compute job's.
fn kept_up(kept: Kept, plan: &Plan, read: Read, with_runs: BTreeSet<usize>, runs: &mut [KeptRun], units: &Units, workflow: &str) -> LaneBook {
    let Kept { lane, runs: was, .. } = kept;
    let marked: HashMap<String, Vec<(Check, String)>> = was.into_iter().map(|k| (k.id, k.marks)).collect();
    for r in runs.iter_mut() {
        r.marks = read.said.get(&r.id).map(|(_, m)| m.clone()).or_else(|| marked.get(&r.id).cloned()).unwrap_or_default();
    }
    let found = findings(&read.shas, &read.bisections, workflow);
    let affects: Vec<Affects> = read.diffs.iter().map(|d| affects_of(units, d.as_deref())).collect();
    let mut l = Arc::try_unwrap(lane).unwrap_or_else(|shared| LaneBook::clone(&shared));
    up(&mut l.book, plan, affects, read.said, runs, found);
    LaneBook { shas: read.shas, runs: with_runs, book: l.book }
}

/// The book made again of what was `read` ([`Making`]): the lane's runs at its commits (`runs`, in its order), each
/// given W16's marks as read.
fn anew(read: Read, runs: &mut [KeptRun], units: Arc<Units>, workflow: &str) -> Making {
    let Read { shas, diffs, mut said, bisections } = read;
    let mut taken: Vec<Said> = Vec::with_capacity(runs.len());
    for r in runs.iter_mut() {
        let (seen, marks) = said.remove(&r.id).unwrap_or_default();
        r.marks.clone_from(&marks);
        taken.push((seen, marks));
    }
    Making::new(units, workflow, shas, diffs, taken, bisections)
}

/// The lane's culprit findings ([`found_of`]) at its commits `shas`, in the bisections' order.
fn findings(shas: &[String], bisections: &[Bisect], workflow: &str) -> Vec<Found> {
    let place: BTreeMap<String, usize> = shas.iter().enumerate().map(|(i, s)| (s.clone(), i)).collect();
    bisections.iter().filter_map(|b| found_of(b, &place, workflow)).collect()
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

/// The Cargo workspace's units at `sha` (W28-R54), with what they're made of (each manifest's path and
/// blob, as `git ls-tree` lists them): `known`'s (a key and its units) where the manifests are those,
/// else made of them, read at once (`git cat-file --batch`) rather than with a `git show` each (the DB
/// query audit, 2026-11-06: kiln's own tree has 93). None when it has no root manifest.
pub(crate) async fn units_at(hub: &Hub, mirror: &str, sha: &str, known: Option<(&str, &Arc<Units>)>) -> (String, Arc<Units>) {
    let listed = git(hub, &["-C", mirror, "ls-tree", "-r", sha]).await.unwrap_or_default();
    // The tree's files, and the manifests among them, listed on the compute pool, off the shards (goal 9, plan
    // 01a1a87b2cec step 3): a tree's lines, ten thousand of them, were made Strings in one poll of the shard.
    let Ok((files, manifests)) = crate::rt::compute(move || listing(&listed)).await else { return (String::new(), Arc::default()) };
    let key: String = manifests.iter().map(|(path, blob)| format!("{blob} {path}\n")).collect();
    if let Some((_, units)) = known.filter(|(k, _)| *k == key) {
        return (key, Arc::clone(units));
    }
    if !manifests.iter().any(|(path, _)| path == "Cargo.toml") {
        return (key, Arc::default());
    }
    let input: Vec<u8> = manifests.iter().flat_map(|(_, blob)| blob.bytes().chain(std::iter::once(b'\n'))).collect();
    let Ok(read) = super::diffs::git_with_input(hub, &["-C", mirror, "cat-file", "--batch"], input).await else { return (key, Arc::default()) };
    let mut texts: BTreeMap<String, String> = manifests.into_iter().zip(blobs(&read)).filter_map(|((path, blob), (got, text))| Some((path, text.filter(|_| got == blob)?))).collect();
    let Some(root) = texts.remove("Cargo.toml") else { return (key, Arc::default()) };
    // The manifests parsed, and the units made, there too.
    let units = crate::rt::compute(move || units_of(&root, &files, &texts)).await.unwrap_or_default();
    (key, Arc::new(units))
}

/// A tree's files as `git ls-tree -r` lists them, and its manifests among them (each `Cargo.toml`, the
/// root's too) with their blobs, in its order.
fn listing(listed: &str) -> (Vec<String>, Vec<(String, String)>) {
    let mut files = Vec::new();
    let mut manifests = Vec::new();
    for line in listed.lines() {
        let Some((meta, path)) = line.split_once('\t') else { continue };
        let mut meta = meta.split(' ');
        let (kind, blob) = (meta.nth(1).unwrap_or_default(), meta.next().unwrap_or_default());
        if kind == "blob" && path.ends_with("Cargo.toml") {
            manifests.push((path.to_owned(), blob.to_owned()));
        }
        files.push(path.to_owned());
    }
    (files, manifests)
}

/// The objects `git cat-file --batch` wrote, in order: each one's name, and its text (none for one it
/// had no such object for: a manifest that can't be read, which leaves its member out).
fn blobs(read: &[u8]) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut rest = read;
    while let Some(nl) = rest.iter().position(|b| *b == b'\n') {
        let header = String::from_utf8_lossy(rest.get(..nl).unwrap_or_default()).into_owned();
        rest = rest.get(nl.saturating_add(1)..).unwrap_or_default();
        let mut words = header.split(' ');
        let (name, kind, size) = (words.next().unwrap_or_default().to_owned(), words.next().unwrap_or_default(), words.next().and_then(|n| n.parse::<usize>().ok()));
        let Some(size) = size.filter(|_| kind == "blob") else {
            out.push((name, None));
            continue;
        };
        out.push((name, Some(String::from_utf8_lossy(rest.get(..size).unwrap_or_default()).into_owned())));
        rest = rest.get(size.saturating_add(1)..).unwrap_or_default();
    }
    out
}

/// The workspace's units from its root manifest, its tree's files and its members' manifests by path.
fn units_of(root: &str, files: &[String], manifests: &BTreeMap<String, String>) -> Units {
    Units::new(kiln_ci::verdict::cargo::units(root, files, |p| manifests.get(p).cloned()).unwrap_or_default())
}

/// What each of commits `shas`' diff against its first parent could affect (W24-R17), in their order
/// ([`diffs_of`], [`affects_of`]).
pub(crate) async fn affects(hub: &Hub, mirror: &str, units: &Units, shas: &[String]) -> Vec<Affects> {
    diffs_of(hub, mirror, shas).await.iter().map(|d| affects_of(units, d.as_deref())).collect()
}

/// Each of commits `shas`' changed files against its first parent, in their order, as `git diff-tree
/// <sha>^ <sha>` lists them (a line a file; no renames: a renamed file's old path and new; a merge's
/// against its first parent): none for one without a parent, or where they can't be read. One `git
/// log` of them (of each [`AT_ONCE`]) rather than a `git diff-tree` each (the DB query audit,
/// 2026-11-06: a lane's 200 commits 6.1 s -> 0.62 s on kiln's own, 59 of them merges, 0.65-0.99 s ->
/// 10-15 ms of small ones, the same files for every one).
async fn diffs_of(hub: &Hub, mirror: &str, shas: &[String]) -> Vec<Option<String>> {
    let mut out = Vec::with_capacity(shas.len());
    for part in shas.chunks(AT_ONCE) {
        let mut args = vec!["-C", mirror, "log", "--no-walk=unsorted", "--diff-merges=first-parent", "--no-renames", "--name-only", "--format=%x00%H %P"];
        args.extend(part.iter().map(String::as_str));
        let log = git(hub, &args).await.unwrap_or_default();
        let changed = changed(&log);
        out.extend(part.iter().map(|sha| changed.get(sha.as_str()).map(|files| files.join("\n"))));
    }
    out
}

/// Each commit of [`diffs_of`]' log that has a parent, with the files it changed.
fn changed(log: &str) -> HashMap<&str, Vec<&str>> {
    log.split('\0')
        .filter_map(|entry| {
            let mut lines = entry.lines();
            let mut head = lines.next()?.split(' ');
            let sha = head.next()?;
            head.next().filter(|p| !p.is_empty())?;
            Some((sha, lines.map(str::trim).filter(|l| !l.is_empty()).collect()))
        })
        .collect()
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

/// A run's test rows of its latest attempt, as [`said_of_rows`] picks them out: suite, name, file, leg, outcome and when.
type TestRow = (String, String, Option<String>, String, String, i64);
/// A run's job rows of its attempt, as [`jobs_of`] reads them: job, conclusion and when it finished.
type JobRow = (String, Option<String>, i64);

/// A test result of a run as [`rows_of`] reads it, with its case (suite, name, file) if it's there.
#[derive(Debug, Clone, PartialEq)]
struct ResultRow {
    repo: String,
    attempt: i64,
    case: Option<(String, String, Option<String>)>,
    leg: String,
    outcome: String,
    at: i64,
    quarantined: i64,
    retried: i64,
}

/// Every test result of runs `ids`, by run, each run's in the order they were kept: in one snapshot,
/// beside the writer, for every run at once (the DB query audit, 2026-11-06: three or four reads a run
/// before, each a turn at the writer). A run's results are kept as its legs report them, before it ends
/// and so before its end refreshes the lane.
fn rows_of(c: &rusqlite::Connection, ids: &[String]) -> anyhow::Result<HashMap<String, Vec<ResultRow>>> {
    let mut out: HashMap<String, Vec<ResultRow>> = HashMap::new();
    let mut st = c.prepare_cached(RAN_TESTS)?;
    let rows = st.query_map([serde_json::to_string(ids)?], |x| {
        let case = match (x.get::<_, Option<String>>(3)?, x.get::<_, Option<String>>(4)?) {
            (Some(suite), Some(name)) => Some((suite, name, x.get(5)?)),
            _ => None,
        };
        Ok((x.get::<_, String>(0)?, ResultRow { repo: x.get(1)?, attempt: x.get(2)?, case, leg: x.get(6)?, outcome: x.get(7)?, at: x.get(8)?, quarantined: x.get(9)?, retried: x.get(10)? }))
    })?;
    for r in rows {
        let (run, row) = r?;
        out.entry(run).or_default().push(row);
    }
    Ok(out)
}

/// Each of `runs`' (each with its attempt) results' count and last row ([`RAN_COUNTS`]): none for one
/// with no results yet.
fn counts_of(c: &rusqlite::Connection, runs: &[(String, i64)]) -> anyhow::Result<HashMap<String, (i64, i64)>> {
    let ids: Vec<&str> = runs.iter().map(|(id, _)| id.as_str()).collect();
    let mut st = c.prepare_cached(RAN_COUNTS)?;
    let rows = st.query_map([serde_json::to_string(&ids)?], |x| Ok((x.get::<_, String>(0)?, (x.get::<_, i64>(1)?, x.get::<_, i64>(2)?))))?;
    Ok(rows.collect::<Result<HashMap<_, _>, _>>()?)
}

/// The v2 jobs of each of `runs`' attempts, by run, each run's in the order they were made.
fn jobs_of(c: &rusqlite::Connection, runs: &[(String, i64)]) -> anyhow::Result<HashMap<String, Vec<JobRow>>> {
    let mut out: HashMap<String, Vec<JobRow>> = HashMap::new();
    let mut st = c.prepare_cached(RAN_JOBS)?;
    let rows = st.query_map([serde_json::to_string(runs)?], |x| Ok((x.get::<_, String>(0)?, (x.get::<_, String>(1)?, x.get::<_, Option<String>>(2)?, x.get::<_, i64>(3)?))))?;
    for r in rows {
        let (run, job) = r?;
        out.entry(run).or_default().push(job);
    }
    Ok(out)
}

/// What `runs` hold for the book, read at once ([`rows_of`]) and made on the compute pool a run at a
/// time, off the shards (goal 9, plan 01a1a87b2cec step 3): each run's results at its place and W16's
/// marks, by run.
async fn said_of(hub: &Hub, repo_id: &str, workflow: &str, units: &Arc<Units>, runs: &[&KeptRun]) -> Result<HashMap<String, Said>, Fail> {
    if runs.is_empty() {
        return Ok(HashMap::new());
    }
    let ids: Vec<String> = runs.iter().map(|r| r.id.clone()).collect();
    let mut rows = hub.store.read(move |c| rows_of(c, &ids)).await.map_err(fail)?;
    let items: Vec<(String, usize, Vec<ResultRow>, Vec<JobRow>)> = runs.iter().map(|r| (r.id.clone(), r.place, rows.remove(&r.id).unwrap_or_default(), r.jobs.clone())).collect();
    let (repo_id, workflow, units) = (repo_id.to_owned(), workflow.to_owned(), Arc::clone(units));
    let said = crate::rt::compute_map(items, move |(id, place, rows, jobs)| {
        let said = said_of_rows(&repo_id, &workflow, &units, place, &rows, jobs);
        (id, said)
    })
    .await
    .map_err(fail)?;
    Ok(said.into_iter().collect())
}

/// A run's rows as the book takes them, at lane place `p`: each test of its latest attempt in
/// `repo_id` (the greatest of all its results', a case gone or not) and each job of its attempt without
/// reports ([`seen_of`]), and W16's marks over every attempt ([`marks_of`]: flaky, as
/// [`super::results::flaky_of`] finds it, and quarantined). On the thread it's called on, a compute job's.
fn said_of_rows(repo_id: &str, workflow: &str, units: &Units, p: usize, rows: &[ResultRow], jobs: Vec<JobRow>) -> Said {
    let latest = rows.iter().map(|t| t.attempt).max();
    let tests: Vec<TestRow> = rows
        .iter()
        .filter(|t| t.repo == repo_id && Some(t.attempt) == latest)
        .filter_map(|t| t.case.clone().map(|(suite, name, file)| (suite, name, file, t.leg.clone(), t.outcome.clone(), t.at)))
        .collect();
    let marked: Vec<MarkRow> = rows.iter().filter_map(|t| t.case.clone().map(|(suite, name, file)| (suite, name, file, t.leg.clone(), t.quarantined))).collect();
    let flaky = super::results::flaky_of(rows.iter().filter_map(|t| t.case.as_ref().map(|(suite, name, _)| (format!("{suite} {name}"), t.leg.clone(), t.outcome.clone(), t.retried))));
    (seen_of(workflow, units, p, tests, jobs), marks_of(workflow, units, marked, flaky))
}

/// A run's rows as its results at lane place `p` ([`said_of_rows`]), on the thread it's called on: a compute job's.
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

/// A run's test rows of every attempt, as [`said_of_rows`] picks them out: suite, name, file, leg and whether it's
/// quarantined.
type MarkRow = (String, String, Option<String>, String, i64);

/// A run's rows as W16's marks ([`said_of_rows`]), `flaky` the tests it was flaky on: sorted, one a check. On the thread it's
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
    refresh_with(hub, &repo, &mirror, (workflow, branch), sha, &l).await;
    // The book let go of on the blocking pool, off the shards (goal 9, plan 01a1a87b2cec step 3), where this was the last
    // hold of it (the lane's kept book let go of meanwhile): it holds a result of each test at each commit with a run,
    // each freed one by one where it's dropped.
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
                .read(move |c| {
                    use rusqlite::OptionalExtension as _;
                    Ok(c.prepare_cached(CULPRIT_RUN)?.query_row(rusqlite::params![rid, w, br, s], |r| r.get::<_, String>(0)).optional()?)
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
    // One statement for the commits, beside the writer.
    let rows: Vec<(String, String)> = hub
        .store
        .read(move |c| {
            use rusqlite::OptionalExtension as _;
            let mut out = Vec::new();
            let mut st = c.prepare_cached(PUSHER_RUN)?;
            for s in &at {
                let row = st.query_row(rusqlite::params![rid, w, b, s], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).optional()?;
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
    /// W24-R17's units each of a lane's commits could affect, from one log of them (the DB query audit,
    /// 2026-11-06), in the order asked: a commit's files against its first parent, a merge's those its
    /// other side brought, a rename both paths; a commit without a parent, or a file no unit takes,
    /// every unit; an empty commit none; and every unit for each where the log can't be read.
    #[test]
    fn w24_r17_a_lane_s_commits_affect_what_their_diffs_touch() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let dir = tempfile::tempdir()?;
            let git = |args: &[&str]| -> anyhow::Result<String> {
                let o = std::process::Command::new("git")
                    .arg("-C")
                    .arg(dir.path())
                    .args(args)
                    .env("GIT_AUTHOR_NAME", "ada")
                    .env("GIT_AUTHOR_EMAIL", "ada@example.com")
                    .env("GIT_COMMITTER_NAME", "ada")
                    .env("GIT_COMMITTER_EMAIL", "ada@example.com")
                    .output()?;
                anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
                Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
            };
            let write = |file: &str, text: &str| -> anyhow::Result<()> {
                let path = dir.path().join(file);
                std::fs::create_dir_all(path.parent().unwrap_or(dir.path()))?;
                Ok(std::fs::write(path, text)?)
            };
            let commit = |said: &str| -> anyhow::Result<String> {
                git(&["add", "-A"])?;
                git(&["commit", "-q", "--allow-empty", "-m", said])?;
                git(&["rev-parse", "HEAD"])
            };
            git(&["init", "-q", "-b", "main"])?;
            for f in ["a/x.rs", "b/y.rs", "README.md"] {
                write(f, f)?;
            }
            let root = commit("root")?;
            write("a/x.rs", "changed")?;
            let in_a = commit("a")?;
            git(&["mv", "b/y.rs", "a/y.rs"])?;
            let moved = commit("moved")?;
            git(&["checkout", "-q", "-b", "side"])?;
            write("b/z.rs", "z")?;
            commit("side")?;
            git(&["checkout", "-q", "main"])?;
            write("a/x.rs", "again")?;
            let again = commit("again")?;
            git(&["merge", "-q", "--no-ff", "-m", "merged", "side"])?;
            let merged = git(&["rev-parse", "HEAD"])?;
            write("README.md", "read me")?;
            let readme = commit("readme")?;
            let empty = commit("empty")?;
            let unit = |name: &str| kiln_ci::land::conflict::Unit { name: name.into(), ecosystem: "cargo".into(), dir: name.into(), reads: BTreeSet::new(), deps: BTreeSet::new() };
            let units = Units::new([unit("a"), unit("b")]);
            let mirror = dir.path().to_string_lossy().into_owned();
            let asked: Vec<String> = [&merged, &root, &readme, &moved, &in_a, &empty, &again].into_iter().cloned().collect();
            let got = affects(&hub, &mirror, &units, &asked).await;
            let of = |names: &[&str]| Affects::Units(names.iter().map(|n| (*n).to_owned()).collect());
            assert_eq!(got, vec![of(&["b"]), Affects::All, Affects::All, of(&["a", "b"]), of(&["a"]), of(&[]), of(&["a"])]);
            // More than one log's worth (a bisection's): each in its place.
            let many: Vec<String> = asked.iter().cycle().take(AT_ONCE + 44).cloned().collect();
            assert_eq!(affects(&hub, &mirror, &units, &many).await, got.iter().cycle().take(AT_ONCE + 44).cloned().collect::<Vec<_>>());
            let unknown = affects(&hub, &mirror, &units, &[in_a.clone(), "0".repeat(40)]).await;
            assert_eq!(unknown, vec![Affects::All, Affects::All], "a log that can't be read");
            assert!(affects(&hub, &mirror, &units, &[]).await.is_empty());
            Ok(())
        })
    }

    /// W28-R55's reads of a lane (the DB query audit, 2026-11-06): its runs from `runs_lane` alone, in
    /// its order, and a commit's runs found by its commit rather than by walking the lane to it; of runs
    /// made in one second, the latest is the greater number and the first the smaller.
    #[test]
    fn w28_r55_lane_reads_are_planned_on_the_index_they_need() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let store = crate::store::Store::in_memory()?;
            let (plans, picked) = store
                .call(|c| {
                    c.execute_batch(
                        "INSERT INTO repos (id, name, clone_url, created_at) VALUES ('r', 'acme/app', 'u', 0);
                         INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, created_at, workflow_id, actor) VALUES
                           ('a', 'r', 7, 'ci', 'push', 'main', 's', 'succeeded', 100, 'w', 'ada'), ('b', 'r', 3, 'ci', 'push', 'main', 's', 'succeeded', 100, 'w', 'sam'),
                           ('c', 'r', 5, 'ci', 'push', 'main', 's', 'succeeded', 90, 'w', 'kim'), ('d', 'r', 9, 'ci', 'push', 'dev', 's', 'succeeded', 200, 'w', 'lee');",
                    )?;
                    let plan = |sql: &str| -> anyhow::Result<String> {
                        let mut st = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                        let n = st.parameter_count();
                        let rows = st.query_map(rusqlite::params_from_iter(std::iter::repeat_n("x", n)), |r| r.get::<_, String>(3))?;
                        Ok(rows.collect::<Result<Vec<_>, _>>()?.join(" | "))
                    };
                    let plans = (plan(LANE_RUNS)?, plan(CULPRIT_RUN)?, plan(PUSHER_RUN)?);
                    let lane = ["r", "w", "main", "s"];
                    let culprit: String = c.query_row(CULPRIT_RUN, lane, |r| r.get(0))?;
                    let pusher: String = c.query_row(PUSHER_RUN, lane, |r| r.get(0))?;
                    let runs: Vec<String> = c.prepare(LANE_RUNS)?.query_map(["r", "w", "main"], |r| r.get(0))?.collect::<Result<_, _>>()?;
                    Ok((plans, (culprit, pusher, runs)))
                })
                .await?;
            let (lane, culprit, pusher) = plans;
            assert!(lane.contains("COVERING INDEX runs_lane") && !lane.contains("TEMP B-TREE"), "{lane}");
            assert!(culprit.contains("runs_repo_sha") && pusher.contains("runs_repo_sha"), "{culprit}; {pusher}");
            assert_eq!(picked, ("a".to_owned(), "kim".to_owned(), vec!["c".to_owned(), "b".to_owned(), "a".to_owned()]));
            Ok(())
        })
    }

    /// One input to a lane's book: a result, or a mark.
    #[derive(Debug, PartialEq)]
    enum Input {
        See(Seen),
        Mark(Check, String),
    }

    /// W28-R55's book of a lane from its runs read at once (the DB query audit, 2026-11-06): each run
    /// gives, in the lane's order, a result for each test of its latest attempt (the greatest of all its
    /// results', a case gone or not) in its repository, one for each job of its attempt without reports
    /// (passed, failed or skipped; not cancelled, not unconcluded, not a v1 job), and marks: quarantined,
    /// or flaky (passed and failed on one leg, or passed on a retry) over every attempt; and how many
    /// results it has, with its last's row, which say whether it changed. Runs with tests and without,
    /// results of another repository's and of a case gone, and a run of another lane among them; each read
    /// on its index.
    #[test]
    fn w28_r55_a_lane_s_runs_read_at_once_make_its_book() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let store = crate::store::Store::in_memory()?;
            let (inputs, counts, plans) = store
                .call(|c| {
                    c.execute_batch(
                        "INSERT INTO repos (id, name, clone_url, created_at) VALUES ('r', 'acme/app', 'u', 0), ('o', 'acme/other', 'u', 0);
                         INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, created_at, workflow_id, attempt) VALUES
                           ('ra', 'r', 1, 'ci', 'push', 'main', 's0', 'failed', 100, 'w', 1), ('rb', 'r', 2, 'ci', 'push', 'main', 's1', 'succeeded', 200, 'w', 2),
                           ('rc', 'r', 3, 'ci', 'push', 'main', 's2', 'succeeded', 300, 'w', 1), ('rd', 'r', 4, 'ci', 'push', 'main', 's2', 'failed', 400, 'w', 1),
                           ('re', 'r', 5, 'ci', 'push', 'main', 's3', 'failed', 500, 'v', 1);
                         INSERT INTO test_cases (repo_id, id, suite, name, file, first_seen) VALUES
                           ('r', 'c1', 'kiln::a', 'adds', 'crates/kiln/src/a.rs', 0), ('r', 'c2', 'kiln::b', 'subtracts', NULL, 0), ('r', 'c3', '', 'bare', NULL, 0),
                           ('r', 'c4', 'kiln::q', 'waits', NULL, 0), ('r', 'c5', 'kiln::f', 'flips', NULL, 0), ('o', 'c1', 'other', 'theirs', NULL, 0);",
                    )?;
                    let jobs = [
                        ("ra", 1, Some("test"), Some("success"), Some(110)), ("ra", 1, Some("build"), Some("failure"), Some(111)), ("ra", 1, Some("lint"), Some("skipped"), None),
                        ("ra", 1, Some("deploy"), None, None), ("ra", 1, None, Some("success"), Some(112)), ("ra", 1, Some("docs"), Some("cancelled"), Some(113)),
                        ("rb", 1, Some("test"), Some("failure"), Some(210)), ("rb", 2, Some("test"), Some("success"), Some(220)), ("rb", 2, Some("build"), Some("timed_out"), Some(221)),
                        ("rc", 1, Some("test"), Some("success"), Some(310)), ("rd", 1, Some("test"), Some("failure"), Some(410)), ("re", 1, Some("test"), Some("failure"), Some(510)),
                    ];
                    for (i, (run, attempt, job, conclusion, finished)) in jobs.into_iter().enumerate() {
                        c.execute(
                            "INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, job_id, conclusion, finished_at, run_attempt) VALUES (?1, ?2, ?3, ?3, 'succeeded', '[]', '[]', '{}', ?4, ?5, ?6, ?7)",
                            rusqlite::params![format!("j{i}"), run, job.unwrap_or("v1"), job, conclusion, finished, attempt],
                        )?;
                    }
                    let results = [
                        ("r", "c1", "ra", "test/0", 1, "passed", 0, 0), ("r", "c2", "ra", "test/0", 1, "failed", 0, 0), ("r", "c3", "ra", "test/1", 1, "error", 0, 0),
                        ("r", "c4", "ra", "test/1", 1, "failed", 0, 1), ("r", "c5", "ra", "test/1", 1, "skipped", 0, 0),
                        ("r", "c1", "rb", "test/0", 1, "failed", 0, 0), ("r", "c1", "rb", "test/0", 2, "passed", 0, 0), ("r", "c5", "rb", "test/1", 2, "passed", 1, 0),
                        ("r", "c2", "rb", "test/1", 2, "failed", 0, 0), ("r", "gone", "rb", "test/1", 2, "failed", 0, 0),
                        ("r", "c1", "rd", "test/0", 1, "passed", 0, 0), ("r", "gone", "rd", "test/0", 3, "failed", 0, 0), ("o", "c1", "rd", "test/0", 1, "failed", 0, 0),
                        ("r", "c2", "re", "test/0", 1, "failed", 0, 0),
                    ];
                    for (i, (repo, case, run, leg, attempt, outcome, retried, quarantined)) in results.into_iter().enumerate() {
                        c.execute(
                            "INSERT INTO test_results (repo_id, case_id, run_id, leg, leg_key, attempt, branch, sha, outcome, retried, quarantined, ts) VALUES (?1, ?2, ?3, ?4, ?4, ?5, 'main', 's', ?6, ?7, ?8, ?9)",
                            rusqlite::params![repo, case, run, leg, attempt, outcome, retried, quarantined, 1_000 + i64::try_from(i)?],
                        )?;
                    }
                    // The lane's runs, as `LANE_RUNS` gives them, each at its commit's place.
                    let lane: Vec<(String, i64, usize)> = c.prepare(LANE_RUNS)?.query_map(["r", "w", "main"], |x| Ok((x.get::<_, String>(0)?, x.get::<_, i64>(2)?, x.get::<_, String>(1)?)))?
                        .map(|r| r.map(|(id, attempt, sha)| (id, attempt, if sha == "s0" { 0 } else if sha == "s1" { 1 } else { 2 })))
                        .collect::<Result<_, _>>()?;
                    let asked: Vec<(String, i64)> = lane.iter().map(|(run, attempt, _)| (run.clone(), *attempt)).collect();
                    let ids: Vec<String> = asked.iter().map(|(id, _)| id.clone()).collect();
                    let (mut rows, mut jobs, counts) = (rows_of(c, &ids)?, jobs_of(c, &asked)?, counts_of(c, &asked)?);
                    let units = Units::default();
                    let mut inputs = Vec::new();
                    for (run, _, p) in &lane {
                        let (seen, marks) = said_of_rows("r", "w", &units, *p, &rows.remove(run).unwrap_or_default(), jobs.remove(run).unwrap_or_default());
                        inputs.extend(seen.into_iter().map(Input::See));
                        inputs.extend(marks.into_iter().map(|(check, why)| Input::Mark(check, why)));
                    }
                    let counts: BTreeMap<String, (i64, i64)> = counts.into_iter().collect();
                    let plan = |sql: &str| -> anyhow::Result<String> { Ok(c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?.query_map(["[]"], |r| r.get::<_, String>(3))?.collect::<Result<Vec<_>, _>>()?.join(" | ")) };
                    Ok((inputs, counts, (plan(RAN_TESTS)?, plan(RAN_JOBS)?, plan(RAN_COUNTS)?)))
                })
                .await?;
            let check = |unit: &str, test: &str| Check { workflow: "w".into(), unit: unit.into(), test: test.into() };
            let seen = |commit: usize, check: Check, outcome: Outcome, at: i64| Input::See(Seen { commit, check, outcome, how: How::Ran, at });
            let failed = |signature: &str| Outcome::Failed { signature: signature.into() };
            let flaky = "flaky: it passed and failed on the same input (W16-R04)";
            assert_eq!(
                inputs,
                vec![
                    // ra: its tests but the skipped one; build and lint, test reported, deploy unconcluded, docs cancelled.
                    seen(0, check("test", "kiln::a adds"), Outcome::Passed, 1000),
                    seen(0, check("test", "kiln::b subtracts"), failed("kiln::b subtracts"), 1001),
                    seen(0, check("test", "bare"), failed(" bare"), 1002),
                    seen(0, check("test", "kiln::q waits"), failed("kiln::q waits"), 1003),
                    seen(0, Check::unit("w", "build"), failed("failure"), 111),
                    seen(0, Check::unit("w", "lint"), Outcome::Skipped, 0),
                    Input::Mark(check("test", "kiln::q waits"), "quarantined (W16-R05)".into()),
                    // rb: its second attempt's tests (not the case gone) and build; adds and flips flaky.
                    seen(1, check("test", "kiln::a adds"), Outcome::Passed, 1006),
                    seen(1, check("test", "kiln::f flips"), Outcome::Passed, 1007),
                    seen(1, check("test", "kiln::b subtracts"), failed("kiln::b subtracts"), 1008),
                    seen(1, Check::unit("w", "build"), failed("timed_out"), 221),
                    Input::Mark(check("test", "kiln::a adds"), flaky.into()),
                    Input::Mark(check("test", "kiln::f flips"), flaky.into()),
                    // rc: no tests, its job.
                    seen(2, Check::unit("w", "test"), Outcome::Passed, 310),
                    // rd: its latest attempt (3, a case gone) has no tests in acme/app: its job.
                    seen(2, Check::unit("w", "test"), failed("failure"), 410),
                ]
            );
            // How many results each has, and its last's row: rc has none; re isn't the lane's.
            let want: BTreeMap<String, (i64, i64)> = [("ra", (5, 5)), ("rb", (5, 10)), ("rd", (3, 13))].into_iter().map(|(r, n)| (r.to_owned(), n)).collect();
            assert_eq!(counts, want);
            let (tests, jobs, counted) = plans;
            assert!(tests.contains("test_results_run") && !tests.contains("SCAN t"), "{tests}");
            assert!(jobs.contains("jobs_run_attempt (run_id=? AND run_attempt=?)"), "{jobs}");
            assert!(counted.contains("COVERING INDEX test_results_run") && !counted.contains("SCAN t"), "{counted}");
            Ok(())
        })
    }

    /// W28-R54's units from the tree's listing (the DB query audit, 2026-11-06): its files, and its manifests with their
    /// blobs, the key the units are known by; the manifests read at once as `git cat-file --batch` writes them, one it
    /// has none of left out.
    #[test]
    fn w28_r54_the_manifests_are_listed_and_read_at_once() {
        let tree = "100644 blob aaaa\tCargo.lock\n100644 blob bbbb\tCargo.toml\n100644 blob cccc\tcrates/app/Cargo.toml\n100644 blob dddd\tcrates/app/src/main.rs\n160000 commit eeee\tvendor/sub\n100644 blob ffff\tcrates/core/Cargo.toml\n";
        let (files, manifests) = listing(tree);
        assert_eq!(files, ["Cargo.lock", "Cargo.toml", "crates/app/Cargo.toml", "crates/app/src/main.rs", "vendor/sub", "crates/core/Cargo.toml"]);
        let blob = |p: &str, b: &str| (p.to_owned(), b.to_owned());
        assert_eq!(manifests, [blob("Cargo.toml", "bbbb"), blob("crates/app/Cargo.toml", "cccc"), blob("crates/core/Cargo.toml", "ffff")]);
        let read = b"bbbb blob 11\n[workspace]\ncccc missing\nffff blob 2\nxy\n";
        assert_eq!(blobs(read), [("bbbb".to_owned(), Some("[workspace]".to_owned())), ("cccc".to_owned(), None), ("ffff".to_owned(), Some("xy".to_owned()))]);
    }

    /// A random number's source for [`w28_r55_a_kept_book_is_the_one_made_again`]: the same every run.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            usize::try_from(self.0 % u64::try_from(n.max(1)).unwrap_or(1)).unwrap_or(0)
        }

        fn pick<'a, T>(&mut self, of: &'a [T]) -> Option<&'a T> {
            of.get(self.below(of.len()))
        }
    }

    /// A run of the lane [`w28_r55_a_kept_book_is_the_one_made_again`] makes: its commit, attempt, results (with their
    /// rows) and each attempt's jobs.
    struct SimRun {
        id: String,
        sha: String,
        attempt: i64,
        rows: Vec<(i64, ResultRow)>,
        jobs: BTreeMap<i64, Vec<JobRow>>,
    }

    /// W28-R55's lane book read again (the DB query audit, 2026-11-06), as it's kept: brought up to date read after read
    /// as the lane moves on (commits gained at its tip and let go of at its start, older commits asked about), its runs
    /// are made, run again, given more results and their jobs' ends, and go, W16's marks come and go, and culprit
    /// findings end, it is after every read the book made again of all of it (`Book::same`): the same commits, checks,
    /// results, marks and findings, so the same verdicts and new failures.
    #[test]
    fn w28_r55_a_kept_book_is_the_one_made_again() -> anyhow::Result<()> {
        let units = Arc::new(units());
        let back = 6_usize;
        let cases = [("my_core::tests", "it", Some("crates/core/src/lib.rs")), ("my_core::tests", "other", None), ("app::tests", "main", Some("crates/app/src/main.rs")), ("app_tests", "flips", None), ("docs", "links", Some("docs/ci.md")), ("", "bare", None)];
        let files = ["crates/core/src/lib.rs", "crates/app/src/main.rs", "docs/ci.md", "crates/core/Cargo.toml"];
        let (outcomes, legs, jobs_of_run, ends) = (["passed", "passed", "failed", "error", "skipped"], ["test/0", "test/1", "lint/0"], ["test", "lint", "build"], [Some("success"), Some("failure"), Some("timed_out"), Some("skipped"), Some("cancelled"), None]);
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut chain: Vec<(String, Option<String>)> = vec![(sha(0), None)];
        let mut runs: Vec<SimRun> = Vec::new();
        let mut bisections: Vec<Bisect> = Vec::new();
        let mut kept: Option<Kept> = None;
        let (mut row, mut made_runs, mut ups, mut reads) = (0_i64, 0_usize, 0_usize, 0_usize);
        for _ in 0..600 {
            match rng.below(10) {
                0 | 1 => {
                    for _ in 0..=rng.below(2) {
                        let diff = (0..=rng.below(2)).filter_map(|_| rng.pick(&files).copied()).collect::<Vec<_>>().join("\n");
                        chain.push((format!("{:040x}", chain.len()), Some(diff)));
                    }
                }
                2 => {
                    // A run at one of the newest commits, or a try at an older one.
                    let from = if rng.below(5) == 0 { 0 } else { chain.len().saturating_sub(3) };
                    let Some((at, _)) = chain.get(from + rng.below(chain.len() - from)) else { continue };
                    made_runs += 1;
                    runs.push(SimRun { id: format!("run{made_runs}"), sha: at.clone(), attempt: 1, rows: Vec::new(), jobs: BTreeMap::new() });
                }
                3..=5 => {
                    let n = runs.len();
                    let Some(r) = runs.get_mut(rng.below(n)) else { continue };
                    for _ in 0..=rng.below(3) {
                        row += 1;
                        let case = (rng.below(12) != 0).then(|| rng.pick(&cases).map(|(s, n, f)| ((*s).to_owned(), (*n).to_owned(), f.map(str::to_owned)))).flatten();
                        let result = ResultRow {
                            repo: if rng.below(10) == 0 { "o".into() } else { "r".into() },
                            attempt: r.attempt,
                            case,
                            leg: rng.pick(&legs).copied().unwrap_or_default().to_owned(),
                            outcome: rng.pick(&outcomes).copied().unwrap_or_default().to_owned(),
                            at: row,
                            quarantined: i64::from(rng.below(8) == 0),
                            retried: i64::from(rng.below(10) == 0),
                        };
                        r.rows.push((row, result));
                    }
                }
                6 => {
                    let n = runs.len();
                    if let Some(r) = runs.get_mut(rng.below(n)) {
                        r.attempt += 1;
                    }
                }
                7 => {
                    let n = runs.len();
                    let Some(r) = runs.get_mut(rng.below(n)) else { continue };
                    let job = rng.pick(&jobs_of_run).copied().unwrap_or_default().to_owned();
                    let end = rng.pick(&ends).copied().flatten().map(str::to_owned);
                    row += 1;
                    let these = r.jobs.entry(r.attempt).or_default();
                    these.retain(|(j, _, _)| *j != job);
                    these.push((job, end, row));
                }
                8 if rng.below(3) == 0 && !runs.is_empty() => {
                    let gone = rng.below(runs.len());
                    runs.retain(|r| r.id != format!("run{}", gone + 1));
                }
                8 => {
                    let n = chain.len();
                    let at = |rng: &mut Rng| chain.get(rng.below(n)).map(|(s, _)| s.clone()).unwrap_or_default();
                    let state = ["found", "no-culprit", "inconclusive", "running"].get(rng.below(4)).copied().unwrap_or_default();
                    let suspects = (0..=rng.below(3)).map(|_| at(&mut rng)).collect();
                    let (test, unit) = match rng.pick(&cases) {
                        Some((s, name, _)) if rng.below(2) == 0 => (Some(format!("{s} {name}").trim().to_owned()), Some("my-core".to_owned())),
                        _ => (None, None),
                    };
                    row += 1;
                    bisections.push(Bisect { state: state.into(), culprit: Some(Culprit { sha: at(&mut rng), ..Culprit::default() }), test, unit, job: "test".into(), suspects, why: Some("its tries disagreed".into()), updated_at: row, ..Bisect::default() });
                }
                _ => {}
            }
            // A read: of the tip mostly, of an older commit now and then, as lane_book takes its commits.
            let listed: Vec<&String> = chain.iter().rev().take(2 * back).map(|(s, _)| s).collect::<Vec<_>>().into_iter().rev().collect();
            let asked = if rng.below(8) == 0 { rng.below(listed.len()) } else { listed.len().saturating_sub(1) };
            let shas: Vec<String> = listed.iter().skip(asked.saturating_sub(back)).map(|s| (*s).clone()).collect();
            let place: BTreeMap<&str, usize> = shas.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();
            let at: Vec<&SimRun> = runs.iter().filter(|r| place.contains_key(r.sha.as_str())).collect();
            let now: Vec<KeptRun> = at
                .iter()
                .map(|r| KeptRun {
                    id: r.id.clone(),
                    attempt: r.attempt,
                    place: place.get(r.sha.as_str()).copied().unwrap_or_default(),
                    results: (i64::try_from(r.rows.len()).unwrap_or_default(), r.rows.last().map_or(0, |(id, _)| *id)),
                    jobs: r.jobs.get(&r.attempt).cloned().unwrap_or_default(),
                    marks: Vec::new(),
                })
                .collect();
            let with_runs: BTreeSet<usize> = now.iter().map(|r| r.place).collect();
            let rows_of_run = |id: &str| -> Vec<ResultRow> { runs.iter().find(|r| r.id == id).map(|r| r.rows.iter().map(|(_, x)| x.clone()).collect()).unwrap_or_default() };
            let said_of = |these: &[&KeptRun]| -> HashMap<String, Said> { these.iter().map(|r| (r.id.clone(), said_of_rows("r", "ci", &units, r.place, &rows_of_run(&r.id), r.jobs.clone()))).collect() };
            let diffs_of = |these: &[String]| -> Vec<Option<String>> { these.iter().map(|s| chain.iter().find(|(c, _)| c == s).and_then(|(_, d)| d.clone())).collect() };
            let anew_of = |now: &mut Vec<KeptRun>| -> LaneBook {
                let diffs = std::iter::once(None).chain(diffs_of(shas.get(1..).unwrap_or_default())).collect();
                let all: Vec<&KeptRun> = now.iter().collect();
                let said = said_of(&all);
                let mut making = anew(Read { shas: shas.clone(), diffs, said, bisections: bisections.clone() }, now, Arc::clone(&units), "ci");
                while making.step() {}
                let (shas, book) = making.done();
                LaneBook { shas, runs: with_runs.clone(), book }
            };
            // As it's made again from scratch.
            let mut fresh = now.clone();
            let again = anew_of(&mut fresh);
            // As it's kept.
            reads += 1;
            let mut runs_now = now.clone();
            let lane = match (kept.take(), kept.as_ref().map(|_| ())) {
                (Some(k), _) => match Plan::of(&k, &shas, &runs_now) {
                    Some(plan) => {
                        ups += 1;
                        let reread: Vec<&KeptRun> = now.iter().filter(|r| plan.places.contains(&r.place)).collect();
                        let read = Read { shas: shas.clone(), diffs: diffs_of(&plan.gained), said: said_of(&reread), bisections: bisections.clone() };
                        kept_up(k, &plan, read, with_runs.clone(), &mut runs_now, &units, "ci")
                    }
                    None => anew_of(&mut runs_now),
                },
                (None, _) => anew_of(&mut runs_now),
            };
            anyhow::ensure!(lane.shas == again.shas && lane.runs == again.runs, "read {reads}: the lane's commits");
            anyhow::ensure!(lane.book.same(&again.book), "read {reads}: the kept book isn't the one made again");
            anyhow::ensure!(lane.book.verdicts() == again.book.verdicts() && lane.book.failures() == again.book.failures(), "read {reads}: the verdicts");
            let marks = |rs: &[KeptRun]| rs.iter().map(|r| (r.id.clone(), r.marks.clone())).collect::<Vec<_>>();
            anyhow::ensure!(marks(&runs_now) == marks(&fresh), "read {reads}: the runs' marks");
            kept = Some(Kept { lane: Arc::new(lane), units: Arc::clone(&units), units_key: String::new(), runs: runs_now });
        }
        anyhow::ensure!(ups > reads / 2, "{ups} of {reads} reads brought the kept book up to date");
        Ok(())
    }

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
    /// files and the manifests among them, then the manifests parsed into units), is what its shard made.
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
            let tree: String = LISTED.lines().enumerate().map(|(i, f)| format!("100644 blob {i:040x}\t{f}\n")).collect();
            let (listed, manifests) = crate::rt::compute(move || listing(&tree)).await?;
            let members: Vec<String> = manifests.into_iter().map(|(path, _)| path).filter(|path| path != "Cargo.toml").collect();
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
