//! Verdicts per commit (docs/ci.md W28-R55), the server's part: a lane's commits from the mirror,
//! what each one's diff could affect (its changed files over the Cargo workspace's units,
//! `kiln_ci::verdict::cargo`), W16's test results as checks (each test's unit from its file, else
//! its suite's crate, else its job; legs without reports as their jobs), W16's flaky marks and
//! quarantine as failures that don't count. `kiln_ci::verdict` decides; this only reads.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use http::StatusCode;
use kiln_ci::land::conflict::Units;
use kiln_ci::verdict::{Affects, Book, Check, Commit, How, Outcome, Seen};
use serde_json::{Value, json};

use super::Fail;
use crate::hub::Hub;

/// The most commits a lane looks back over, from the commit asked about.
const BACK: usize = 200;

fn fail(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

async fn git(hub: &Hub, args: &[&str]) -> Option<String> {
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
        if let Some(v) = lane(hub, &repo.id, &mirror, &workflow, &branch, &full).await? {
            out.push(json!({ "workflow": workflow, "branch": branch, "verdict": v }));
        }
    }
    Ok(json!({ "repo": repo.name, "sha": full, "lanes": out }))
}

/// The commit's verdict on one lane, if the lane's first parents hold it.
async fn lane(hub: &Hub, repo_id: &str, mirror: &str, workflow: &str, branch: &str, sha: &str) -> Result<Option<Value>, Fail> {
    // The lane's commits: first parents up to the newest run's, back to the commit and BACK more.
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
    let head = runs.last().map(|r| r.sha.clone()).unwrap_or_else(|| sha.to_owned());
    let listed = git(hub, &["-C", mirror, "rev-list", "--first-parent", &format!("--max-count={}", BACK * 2), &head]).await.unwrap_or_default();
    let mut shas: Vec<String> = listed.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect();
    shas.reverse();
    let Some(at) = shas.iter().position(|s| s == sha) else { return Ok(None) };
    let start = at.saturating_sub(BACK);
    let shas: Vec<String> = shas.into_iter().skip(start).collect();
    // The workspace's units, as the newest commit has them.
    let files: Vec<String> = git(hub, &["-C", mirror, "ls-tree", "-r", "--name-only", &head]).await.unwrap_or_default().lines().map(str::to_owned).collect();
    let root = git(hub, &["-C", mirror, "show", &format!("{head}:Cargo.toml")]).await;
    let mut manifests: BTreeMap<String, String> = BTreeMap::new();
    for f in files.iter().filter(|f| f.ends_with("Cargo.toml") && f.as_str() != "Cargo.toml") {
        if let Some(t) = git(hub, &["-C", mirror, "show", &format!("{head}:{f}")]).await {
            manifests.insert(f.clone(), t);
        }
    }
    let units = Units::new(root.and_then(|r| kiln_ci::verdict::cargo::units(&r, &files, |p| manifests.get(p).cloned()).ok()).unwrap_or_default());
    let mut book = Book::default();
    for (i, s) in shas.iter().enumerate() {
        let changed = match i {
            0 => None,
            _ => git(hub, &["-C", mirror, "diff-tree", "--no-commit-id", "--name-only", "-r", &format!("{s}^"), s]).await,
        };
        let affects = match changed {
            None => Affects::All,
            Some(c) => {
                let files: Vec<&str> = c.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
                if files.iter().any(|f| units.takes(f).is_empty()) { Affects::All } else { Affects::Units(units.affected(files)) }
            }
        };
        book.push(Commit { id: s.clone(), at: 0, affects });
    }
    let place: BTreeMap<&str, usize> = shas.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();
    for run in runs.iter().filter(|r| place.contains_key(r.sha.as_str())) {
        let Some(&p) = place.get(run.sha.as_str()) else { continue };
        for s in results(hub, repo_id, workflow, &run.id, run.attempt, &units, p).await? {
            book.see(s);
        }
        for (check, why) in marks(hub, &run.id, workflow, &units).await {
            book.mark(check, why);
        }
    }
    let Some(&p) = place.get(sha) else { return Ok(None) };
    let mut v = book.verdict(p).and_then(|v| serde_json::to_value(v).ok());
    if let Some(v) = v.as_mut() {
        named(v, &shas);
    }
    Ok(v)
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

/// A test's unit: its file's, else its suite's crate when a unit has that name, else its job.
fn unit_of(units: &Units, file: Option<&str>, suite: &str, job: &str) -> String {
    file.and_then(|f| units.takes(f).into_iter().next().map(str::to_owned))
        .or_else(|| suite.split([':', ' ', '/']).next().filter(|s| units.unit(s).is_some() || units.unit(&s.replace('_', "-")).is_some()).map(|s| units.unit(s).map_or_else(|| s.replace('_', "-"), |_| s.to_owned())))
        .unwrap_or_else(|| job.to_owned())
}

/// A run's results at lane place `p`: each test of its latest attempt, and each job without reports.
async fn results(hub: &Hub, repo_id: &str, workflow: &str, run: &str, attempt: i64, units: &Units, p: usize) -> Result<Vec<Seen>, Fail> {
    let (r, rid) = (run.to_owned(), repo_id.to_owned());
    #[allow(clippy::type_complexity)]
    let (tests, jobs): (Vec<(String, String, Option<String>, String, String, i64)>, Vec<(String, Option<String>, i64)>) = hub
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
    Ok(out)
}

/// W16's marks on a run's tests: flaky (W16-R04's first rule) and quarantined (W16-R05).
async fn marks(hub: &Hub, run: &str, workflow: &str, units: &Units) -> Vec<(Check, String)> {
    let r = run.to_owned();
    let tests: Vec<(String, String, Option<String>, String, i64)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT k.suite, k.name, k.file, t.leg_key, t.quarantined FROM test_results t JOIN test_cases k ON k.repo_id = t.repo_id AND k.id = t.case_id WHERE t.run_id = ?1")?;
            let rows = st.query_map([&r], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap_or_default();
    let flaky: BTreeSet<String> = super::results::flaky_in_run(hub, run).await.unwrap_or_default().into_iter().collect();
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
