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
    let units = units_at(hub, mirror, &head).await;
    let mut book = Book::default();
    for (i, s) in shas.iter().enumerate() {
        let affects = if i == 0 { Affects::All } else { affects(hub, mirror, &units, s).await };
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
    // W28-R57: the lane's culprit findings, its bisections that ended.
    for b in super::bisect::of_lane(hub, repo, workflow, branch).await {
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
            Some(t) => Check { workflow: workflow.to_owned(), unit: b.unit.clone().unwrap_or_else(|| b.job.clone()), test: t.clone() },
            None => Check::unit(workflow, &b.job),
        };
        book.found(Found { check, suspects, finding, at: b.updated_at });
    }
    let with_runs: BTreeSet<usize> = runs.iter().filter_map(|r| place.get(r.sha.as_str()).copied()).collect();
    Ok(Some(LaneBook { shas, runs: with_runs, book }))
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
    let files: Vec<String> = git(hub, &["-C", mirror, "ls-tree", "-r", "--name-only", sha]).await.unwrap_or_default().lines().map(str::to_owned).collect();
    let Some(root) = git(hub, &["-C", mirror, "show", &format!("{sha}:Cargo.toml")]).await else { return Units::default() };
    let mut manifests: BTreeMap<String, String> = BTreeMap::new();
    for f in files.iter().filter(|f| f.ends_with("Cargo.toml") && f.as_str() != "Cargo.toml") {
        if let Some(t) = git(hub, &["-C", mirror, "show", &format!("{sha}:{f}")]).await {
            manifests.insert(f.clone(), t);
        }
    }
    Units::new(kiln_ci::verdict::cargo::units(&root, &files, |p| manifests.get(p).cloned()).unwrap_or_default())
}

/// What commit `sha`'s diff against its first parent could affect (W24-R17): a file no unit
/// takes, or a diff that can't be read, affects every unit.
pub(crate) async fn affects(hub: &Hub, mirror: &str, units: &Units, sha: &str) -> Affects {
    let Some(changed) = git(hub, &["-C", mirror, "diff-tree", "--no-commit-id", "--name-only", "-r", &format!("{sha}^"), sha]).await else { return Affects::All };
    let files: Vec<&str> = changed.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if files.iter().any(|f| units.takes(f).is_empty()) { Affects::All } else { Affects::Units(units.affected(files)) }
}

/// A test's unit: its file's, else its suite's crate when a unit has that name, else its job.
pub(crate) fn unit_of(units: &Units, file: Option<&str>, suite: &str, job: &str) -> String {
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
    let mut rx = hub.events.subscribe();
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
        // A verdict of this commit changed, a run ended, or 30 s went by: look again.
        let nap = left.min(Duration::from_secs(30));
        let mut heard = false;
        while !heard {
            futures::select_biased! {
                () = hub.shutdown.cancelled().fuse() => return Ok(answer(first, true)),
                () = crate::rt::sleep(nap).fuse() => break,
                m = rx.recv().fuse() => match m {
                    Ok(text) => {
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
    let mut rx = hub.events.subscribe();
    loop {
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            m = rx.recv().fuse() => match m {
                Ok(text) => {
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
    let Some(at) = l.shas.iter().position(|s| s == sha) else { return };
    // W28-R57: culprit finding for each new failure with two or more suspects, once declared.
    if hub.cfg.ci.culprits {
        let started = super::bisect::of_lane(hub, &repo.name, workflow, branch).await;
        for f in l.book.failures().into_iter().filter(|f| f.suspects.len() >= 2) {
            let Some(bad) = l.shas.get(f.commit).cloned() else { continue };
            let test = (!f.check.test.is_empty()).then(|| f.check.test.clone());
            if started.iter().any(|b| b.bad == bad && b.test == test && (test.is_some() || b.job == f.check.unit)) {
                continue;
            }
            let (rid, w, br, s) = (repo.id.clone(), workflow.to_owned(), branch.to_owned(), bad.clone());
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
                super::bisect::start_auto(hub, &run, &f.check).await;
            }
        }
    }
    // The commits the run at `sha` newly answers: after the lane's last run before it.
    let after = l.runs.range(..at).next_back().map_or(0, |p| p + 1);
    for (c, commit) in l.shas.iter().enumerate().skip(at.saturating_sub(RECENT)) {
        let Some(v) = l.book.verdict(c) else { continue };
        let key = format!("ci/verdict/{}/{workflow}/{branch}/{commit}", repo.name);
        let kept = format!("{:?}: {}", v.kind, v.why);
        let before = hub.store.kv_get(key.clone()).await.ok().flatten();
        if before.as_deref() == Some(kept.as_str()) {
            continue;
        }
        if hub.store.kv_set(key, kept).await.is_err() {
            continue;
        }
        let verdict = l.named(&v);
        hub.emit(&crate::wire::events::ServerEvent::CiVerdict(Box::new(crate::wire::events::CiVerdict {
            repo: repo.name.clone(),
            workflow: workflow.to_owned(),
            branch: branch.to_owned(),
            sha: commit.clone(),
            kind: v.kind,
            why: v.why.clone(),
            verdict,
        })));
        let news = match &before {
            Some(_) => true,
            None => (after..=at).contains(&c),
        };
        // The push that brought it: the lane's first run at or after it.
        let pushed = l.runs.range(c..).next().and_then(|p| l.shas.get(*p)).map_or(commit.as_str(), String::as_str);
        if news && v.kind != Kind::Pending
            && let Some(agent) = pusher(hub, &repo.id, &mirror, commit, pushed).await
        {
            crate::workspace::follow::deliver(hub, &agent, message(&repo.name, workflow, branch, commit, &v, &l.shas)).await;
        }
    }
}

/// The agent that pushed a commit, as step 6 attributes it (W28-R67, `kiln_ci::sched::actor`):
/// the agent token its push came with (the actor of the run of `pushed`, the push that brought
/// it), the agent change request whose last pushed commit it is, or its `Kiln-Agent` line.
async fn pusher(hub: &Hub, repo_id: &str, mirror: &str, sha: &str, pushed: &str) -> Option<String> {
    use kiln_ci::sched::actor;
    let message = git(hub, &["-C", mirror, "log", "-1", "--format=%B", sha]).await.unwrap_or_default();
    let (rid, at) = (repo_id.to_owned(), pushed.to_owned());
    let by: String = hub
        .store
        .call(move |c| {
            use rusqlite::OptionalExtension as _;
            Ok(c.query_row("SELECT actor FROM runs WHERE repo_id = ?1 AND sha = ?2 ORDER BY created_at LIMIT 1", rusqlite::params![rid, at], |r| r.get::<_, String>(0)).optional()?)
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let changes: Vec<crate::workspace::change::Record> = hub.store.kv_prefix("agents/change/".into()).await.unwrap_or_default().iter().filter_map(|(_, v)| serde_json::from_str(v).ok()).collect();
    let refs: Vec<actor::Change> = changes.iter().map(|c| actor::Change { agent: &c.agent, sha: &c.sha, change: c.change.as_deref() }).collect();
    let who = actor::attribute(&actor::Push { sha, pusher: &by, message: &message }, &refs);
    who.id.strip_prefix("agent:").filter(|id| !id.is_empty()).map(str::to_owned)
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
