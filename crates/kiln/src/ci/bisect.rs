//! Bisecting a failure (docs/ci.md §4.1's `ci.runs.bisect`, W20-R09's `diagnose`;
//! docs/ci-agents.md): a failed run's job, run again on the commits between the last one it passed
//! on and the run's own, until the first commit it fails on is found. `kiln_ci::verdict::culprit`
//! picks each try (W28-R56: a binary search when the check isn't flaky). Each try is an ordinary
//! run of that job (and the jobs it needs) at that commit, on the run's branch, trusted as the
//! failed run was: the commits are its own history. Given a test (W28-R57), only the commits whose
//! diffs could affect its unit are tried, each try judged by the test's own result, with
//! `vars.KILN_VERDICT_UNITS` and `vars.KILN_VERDICT_TESTS` set so a workflow can run just those. A
//! try that can't run (it didn't start, it was cancelled) is skipped for its nearest neighbour; the
//! bisection ends inconclusive when none is left, saying why.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use http::StatusCode;
use rusqlite::OptionalExtension as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use kiln_ci::verdict::culprit::{At, Next, Search};

use super::Fail;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

/// One commit a bisection tried.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Tried {
    pub sha: String,
    /// Its run of the job.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// The run's conclusion (`success`, `failure`, …), or `untestable`.
    pub conclusion: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// The commit a bisection found.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Culprit {
    pub sha: String,
    pub message: String,
    pub author: String,
}

/// A bisection.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Bisect {
    pub id: String,
    /// The failed run, and its repository, workflow, branch and the job bisected.
    pub run: String,
    pub repo: String,
    pub workflow: String,
    pub branch: String,
    pub job: String,
    /// The last commit the job passed on, the first it failed on, and the commits from just after
    /// the one to the other, oldest first (`bad` last).
    pub good: String,
    pub bad: String,
    pub commits: Vec<String>,
    /// The test bisected (W16's `suite name`) and its unit (W28-R57): tried only at `suspects`, the
    /// commits whose diffs could affect the unit, each try judged by the test's own result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub suspects: Vec<String>,
    pub tried: Vec<Tried>,
    /// `running`, `found` (`culprit`), `no-culprit` (the failure didn't come back: a flake),
    /// `inconclusive` or `failed` (`why`).
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub culprit: Option<Culprit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The most commits a bisection takes on: about 20 tries.
const MOST: usize = 1 << 20;
/// How long one try may run.
const TRY: Duration = Duration::from_secs(3 * 3600);

fn key(id: &str) -> String {
    format!("ci/bisect/{id}")
}

impl Bisect {
    /// The commits it searches: its suspects, or for a job every commit between.
    pub fn suspects(&self) -> &[String] {
        if self.suspects.is_empty() { &self.commits } else { &self.suspects }
    }
}

/// A lane's bisections: its repository's, workflow's and branch's (W28-R57).
pub async fn of_lane(hub: &Hub, repo: &str, workflow: &str, branch: &str) -> Vec<Bisect> {
    hub.store
        .kv_prefix("ci/bisect/".to_owned())
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, t)| serde_json::from_str::<Bisect>(&t).ok())
        .filter(|b| b.repo == repo && b.workflow == workflow && b.branch == branch)
        .collect()
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn conflict(m: impl Into<String>) -> Fail {
    (StatusCode::CONFLICT, m.into())
}

async fn save(hub: &Hub, b: &mut Bisect) {
    b.updated_at = crate::util::now_unix();
    if let Ok(text) = serde_json::to_string(&*b)
        && let Err(e) = hub.store.kv_set(key(&b.id), text).await
    {
        tracing::warn!(bisect = %b.id, error = %format!("{e:#}"), "a bisection couldn't be kept");
    }
}

/// A run's bisections, newest first.
pub async fn of_run(hub: &Hub, run: &str) -> Vec<Bisect> {
    let mut v: Vec<Bisect> =
        hub.store.kv_prefix("ci/bisect/".to_owned()).await.unwrap_or_default().into_iter().filter_map(|(_, t)| serde_json::from_str::<Bisect>(&t).ok()).filter(|b| b.run == run).collect();
    v.sort_by_key(|b| std::cmp::Reverse(b.created_at));
    v
}

/// What the run is: its repository's id and name, workflow, branch, commit, trust and when.
struct RunFacts {
    repo_id: String,
    repo: String,
    workflow: String,
    branch: String,
    sha: String,
    trusted: bool,
    created_at: i64,
    number: i64,
}

async fn run_facts(hub: &Hub, run: &str) -> Result<RunFacts, Fail> {
    let id = run.to_owned();
    hub.store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT r.repo_id, p.name, COALESCE(r.workflow_id, ''), r.branch, r.sha, r.trust, r.created_at, r.number FROM runs r JOIN repos p ON p.id = r.repo_id WHERE r.id = ?1",
                [&id],
                |x| {
                    Ok(RunFacts {
                        repo_id: x.get(0)?,
                        repo: x.get(1)?,
                        workflow: x.get(2)?,
                        branch: x.get(3)?,
                        sha: x.get(4)?,
                        trusted: x.get::<_, String>(5)? == "trusted",
                        created_at: x.get(6)?,
                        number: x.get(7)?,
                    })
                },
            )
            .optional()?)
        })
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no run `{run}`")))
}

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `POST /api/ci/runs/:id/bisect { job?, good?, bad? }`: a bisection of the run's `job` (the one
/// that failed, when one did), between `good` (the last green run of its workflow on its branch
/// before it, by default) and `bad` (the run's commit), started by `by`.
pub async fn start(hub: &Arc<Hub>, run: &str, body: &Value, by: &str) -> Result<Response, Fail> {
    if !hub.ci.enabled() {
        return Err((StatusCode::NOT_FOUND, "the CI engine is off ([ci] enabled)".into()));
    }
    let f = run_facts(hub, run).await?;
    if f.workflow.is_empty() {
        return Err(conflict("a v1 pipeline's run isn't bisected: its workflows' are"));
    }
    let text = |k: &str| body.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    // The job: the one asked for, or the one that failed.
    let (rid, asked) = (run.to_owned(), text("job"));
    let failed: Vec<String> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT DISTINCT job_id FROM jobs WHERE run_id = ?1 AND job_id IS NOT NULL AND run_attempt = (SELECT attempt FROM runs WHERE id = ?1) AND conclusion IN ('failure', 'timed_out')",
            )?;
            let rows = st.query_map([&rid], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(internal)?
        .into_iter()
        .map(|j| j.split('/').next().unwrap_or(&j).to_owned())
        .collect();
    // W28-R57: given a test, the job that reported it in this run, when none is asked for.
    let test = text("test");
    let reported: Option<String> = match (&test, &asked) {
        (Some(t), None) => {
            let (rid, want) = (run.to_owned(), t.clone());
            hub.store
                .call(move |c| {
                    Ok(c.query_row(
                        "SELECT t.leg_key FROM test_results t JOIN test_cases k ON k.repo_id = t.repo_id AND k.id = t.case_id WHERE t.run_id = ?1 AND (TRIM(k.suite || ' ' || k.name) = ?2 OR k.name = ?2 OR k.id = ?2) ORDER BY t.attempt DESC LIMIT 1",
                        rusqlite::params![rid, want],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?)
                })
                .await
                .map_err(internal)?
                .map(|leg| leg.split('/').next().unwrap_or(&leg).to_owned())
        }
        _ => None,
    };
    let job = match (asked, reported, failed.as_slice()) {
        (Some(j), _, _) => j,
        (None, Some(j), _) => j,
        (None, None, [one]) => one.clone(),
        (None, None, []) => return Err(conflict("none of the run's jobs failed: say which to bisect (`job`)")),
        (None, None, many) => return Err(conflict(format!("{} failed: say which to bisect (`job`)", many.join(", ")))),
    };
    let bad = text("bad").unwrap_or_else(|| f.sha.clone());
    // Given a test, the last run of the lane it passed in (W28-R57); else the last green run.
    let passed_at: Option<String> = match &test {
        Some(t) => {
            let (repo_id, wf, branch, at, this, want) = (f.repo_id.clone(), f.workflow.clone(), f.branch.clone(), f.created_at, run.to_owned(), t.clone());
            hub.store
                .call(move |c| {
                    Ok(c.query_row(
                        "SELECT r.sha FROM runs r JOIN test_results t ON t.run_id = r.id JOIN test_cases k ON k.repo_id = t.repo_id AND k.id = t.case_id WHERE r.repo_id = ?1 AND r.workflow_id = ?2 AND r.branch = ?3 AND r.id != ?5 AND r.created_at <= ?4 AND t.outcome = 'passed' AND (TRIM(k.suite || ' ' || k.name) = ?6 OR k.name = ?6 OR k.id = ?6) ORDER BY r.created_at DESC LIMIT 1",
                        rusqlite::params![repo_id, wf, branch, at, this, want],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?)
                })
                .await
                .map_err(internal)?
        }
        None => None,
    };
    let good = match text("good").or(passed_at) {
        Some(g) => g,
        None => {
            let (repo_id, wf, branch, at, this) = (f.repo_id.clone(), f.workflow.clone(), f.branch.clone(), f.created_at, run.to_owned());
            hub.store
                .call(move |c| {
                    Ok(c.query_row(
                        "SELECT sha FROM runs WHERE repo_id = ?1 AND workflow_id = ?2 AND branch = ?3 AND conclusion = 'success' AND id != ?5 AND created_at <= ?4 ORDER BY created_at DESC LIMIT 1",
                        rusqlite::params![repo_id, wf, branch, at, this],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?)
                })
                .await
                .map_err(internal)?
                .ok_or_else(|| conflict(format!("no green run of {} on {} before this one: say the last commit it passed on (`good`)", f.workflow, f.branch)))?
        }
    };
    if !is_sha(&good) || !is_sha(&bad) {
        return Err((StatusCode::BAD_REQUEST, "`good` and `bad` are commits' 40 hex digits".into()));
    }
    let mirror = crate::gitops::mirror_path(hub, &f.repo).to_string_lossy().into_owned();
    let git = |args: Vec<String>| {
        let hub = Arc::clone(hub);
        async move {
            let a: Vec<&str> = args.iter().map(String::as_str).collect();
            crate::gitops::run_git(&hub, &a, crate::gitops::LS_REMOTE_TIMEOUT).await
        }
    };
    if git(vec!["-C".into(), mirror.clone(), "merge-base".into(), "--is-ancestor".into(), good.clone(), bad.clone()]).await.is_err() {
        return Err(conflict(format!("{} isn't an ancestor of {} here", good.get(..12).unwrap_or(&good), bad.get(..12).unwrap_or(&bad))));
    }
    let out = git(vec!["-C".into(), mirror.clone(), "rev-list".into(), "--reverse".into(), "--first-parent".into(), "--ancestry-path".into(), format!("{good}..{bad}")]).await.map_err(internal)?;
    let commits: Vec<String> = String::from_utf8_lossy(&out).lines().map(str::trim).filter(|l| is_sha(l)).map(str::to_owned).collect();
    if commits.is_empty() {
        return Err(conflict("there's nothing between them to bisect"));
    }
    if commits.len() > MOST {
        return Err(conflict(format!("{} commits are more than a bisection takes on ({MOST}): say a later `good`", commits.len())));
    }
    // W28-R57: a test, tried only where its unit could have changed, judged by its own result.
    let (mut unit, mut suspects, mut marked) = (None, Vec::new(), false);
    if let Some(t) = &test {
        let (rid, want) = (f.repo_id.clone(), t.clone());
        let case: Option<(String, Option<String>)> = hub
            .store
            .call(move |c| {
                Ok(c.query_row(
                    "SELECT suite, file FROM test_cases WHERE repo_id = ?1 AND (TRIM(suite || ' ' || name) = ?2 OR name = ?2 OR id = ?2) ORDER BY first_seen DESC LIMIT 1",
                    rusqlite::params![rid, want],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                )
                .optional()?)
            })
            .await
            .map_err(internal)?;
        let Some((suite, file)) = case else { return Err(conflict(format!("no test `{t}` has results in {}", f.repo))) };
        let units = super::verdict::units_at(hub, &mirror, &bad).await;
        let u = super::verdict::unit_of(&units, file.as_deref(), &suite, &job);
        for sha in &commits {
            if super::verdict::affects(hub, &mirror, &units, sha).await.unit(&u) {
                suspects.push(sha.clone());
            }
        }
        if suspects.is_empty() {
            return Err(conflict(format!("none of the {} commits between could affect {u}, `{t}`'s unit", commits.len())));
        }
        marked = super::results::flaky_in_run(hub, run).await.unwrap_or_default().iter().any(|n| n == t);
        unit = Some(u);
    }
    let now = crate::util::now_unix();
    let mut b = Bisect {
        id: crate::util::new_id(),
        run: run.to_owned(),
        repo: f.repo.clone(),
        workflow: f.workflow.clone(),
        branch: f.branch.clone(),
        job,
        good,
        bad,
        commits,
        test,
        unit,
        suspects,
        state: "running".into(),
        by: by.to_owned(),
        created_at: now,
        ..Bisect::default()
    };
    save(hub, &mut b).await;
    hub.store.audit(by.to_owned(), "ci.runs.bisect".into(), format!("{} run #{}: {} between {} and {} ({} commits)", f.repo, f.number, b.job, b.good, b.bad, b.commits.len())).await;
    let answer = b.clone();
    let hub = Arc::clone(hub);
    let (repo, workflow, branch, bad) = (b.repo.clone(), b.workflow.clone(), b.branch.clone(), b.bad.clone());
    crate::rt::spawn(async move {
        drive(Arc::clone(&hub), b, f.trusted, marked).await;
        // W28-R59: its end is a culprit finding: the lane's verdicts again, and the news.
        super::verdict::refresh(&hub, &repo, &workflow, &branch, &bad).await;
    });
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "bisect": answer })))
}

/// Searches `b`'s suspects (W28-R56) until the planner names the first commit the check fails on,
/// or no culprit (a flake), or can't tell.
async fn drive(hub: Arc<Hub>, mut b: Bisect, trusted: bool, marked: bool) {
    let Some(repo) = hub.store.repos().await.ok().and_then(|r| r.into_iter().find(|r| r.name == b.repo)) else {
        b.state = "failed".into();
        b.why = Some(format!("{} is gone", b.repo));
        save(&hub, &mut b).await;
        return;
    };
    let suspects: Vec<String> = b.suspects().to_vec();
    let mut search = Search::new(suspects.len(), 0.0, marked);
    let mut untestable: BTreeSet<usize> = BTreeSet::new();
    loop {
        let (at, sha) = match search.next() {
            Next::Try { at: At::Failing } => (At::Failing, b.bad.clone()),
            Next::Try { at: At::Suspect(k) } => {
                // An untestable suspect gives way to its nearest testable neighbour.
                let near = std::iter::once(Some(k)).chain((1..suspects.len()).flat_map(|d| [k.checked_sub(d), k.checked_add(d)])).flatten().find(|j| *j < suspects.len() && !untestable.contains(j));
                match near.and_then(|j| suspects.get(j).map(|s| (j, s.clone()))) {
                    Some((j, s)) => (At::Suspect(j), s),
                    None => {
                        b.state = "inconclusive".into();
                        b.why = Some("no suspect could be tested".into());
                        save(&hub, &mut b).await;
                        return;
                    }
                }
            }
            Next::Culprit { suspect, .. } => {
                let sha = suspects.get(suspect).cloned().unwrap_or_else(|| b.bad.clone());
                let (message, author) = crate::gitops::commit_info(&hub, &repo, &sha).await;
                b.culprit = Some(Culprit { sha, message, author });
                b.state = "found".into();
                save(&hub, &mut b).await;
                return;
            }
            Next::NoCulprit { .. } => {
                b.state = "no-culprit".into();
                b.why = Some("the failure didn't come back when tried again: a flake".into());
                save(&hub, &mut b).await;
                return;
            }
            Next::Inconclusive { why } => {
                b.state = "inconclusive".into();
                b.why = Some(why);
                save(&hub, &mut b).await;
                return;
            }
        };
        // W24's ledger, once it is on main, answers a try whose inputs already ran (W28-R57).
        let check = kiln_ci::verdict::Check { workflow: b.workflow.clone(), unit: b.unit.clone().unwrap_or_else(|| b.job.clone()), test: b.test.clone().unwrap_or_default() };
        let ledger: &dyn kiln_ci::verdict::Ledger = &kiln_ci::verdict::NoLedger;
        let t = match ledger.outcome(&check, &sha) {
            Some(o) => Tried {
                sha: sha.clone(),
                run: None,
                conclusion: if matches!(o, kiln_ci::verdict::Outcome::Failed { .. }) { "failure" } else { "success" }.into(),
                why: Some("its result with these inputs, from the ledger".into()),
            },
            None => try_commit(&hub, &repo, &b, &sha, trusted).await,
        };
        let failed = match t.conclusion.as_str() {
            "success" => Some(false),
            "failure" | "timed_out" => Some(true),
            _ => None,
        };
        b.tried.push(t.clone());
        match (failed, at) {
            (Some(f), _) => search.observe(at, f),
            (None, At::Suspect(k)) => {
                untestable.insert(k);
            }
            (None, At::Failing) => {
                b.state = "inconclusive".into();
                b.why = Some(format!("the failing commit couldn't be tried again: {}", t.why.unwrap_or_else(|| t.conclusion.clone())));
                save(&hub, &mut b).await;
                return;
            }
        }
        save(&hub, &mut b).await;
    }
}

/// The job run at `sha`, to its end: its conclusion, or given a test, the test's own result in it.
async fn try_commit(hub: &Arc<Hub>, repo: &crate::store::Repo, b: &Bisect, sha: &str, trusted: bool) -> Tried {
    let mut vars = BTreeMap::new();
    // W28-R57: a gate's tries go at once, at gate priority; the rest at low priority.
    let gate = hub.cfg.ci.culprit_gates.contains(&b.branch);
    vars.insert("KILN_TRY_PRIORITY".to_owned(), if gate { "90" } else { "20" }.to_owned());
    if let (Some(test), Some(unit)) = (&b.test, &b.unit) {
        vars.insert("KILN_VERDICT_UNITS".to_owned(), unit.clone());
        vars.insert("KILN_VERDICT_TESTS".to_owned(), test.clone());
    }
    let ask = super::uploads::Ask {
        sha: sha.to_owned(),
        git_ref: b.branch.clone(),
        event: "push".into(),
        workflows: vec![b.workflow.clone()],
        job: Some(b.job.clone()),
        vars,
        ..super::uploads::Ask::default()
    };
    let untestable = |why: String| Tried { sha: sha.to_owned(), run: None, conclusion: "untestable".into(), why: Some(why) };
    let started = match super::uploads::start_runs(hub, repo, &ask, trusted, &b.by, false).await {
        Ok((runs, skipped)) => match runs.first().and_then(|r| r.get("id")).and_then(Value::as_str) {
            Some(id) => id.to_owned(),
            None => return untestable(skipped.first().and_then(|s| s.get("why")).and_then(Value::as_str).unwrap_or("its run didn't start").to_owned()),
        },
        Err((_, why)) => return untestable(why),
    };
    let deadline = std::time::Instant::now() + TRY;
    loop {
        let id = started.clone();
        let conclusion: Option<String> =
            hub.store.call(move |c| Ok(c.query_row("SELECT conclusion FROM runs WHERE id = ?1", [&id], |r| r.get::<_, Option<String>>(0)).optional()?.flatten())).await.unwrap_or(None);
        if let Some(c) = conclusion {
            let Some(test) = b.test.clone() else { return Tried { sha: sha.to_owned(), run: Some(started), conclusion: c, why: None } };
            // The test's own result in the try's latest attempt (W28-R57).
            let id = started.clone();
            let want = test.clone();
            let outcome: Option<String> = hub
                .store
                .call(move |c| {
                    Ok(c.query_row(
                        "SELECT t.outcome FROM test_results t JOIN test_cases k ON k.repo_id = t.repo_id AND k.id = t.case_id WHERE t.run_id = ?1 AND (TRIM(k.suite || ' ' || k.name) = ?2 OR k.name = ?2 OR k.id = ?2) ORDER BY t.attempt DESC LIMIT 1",
                        rusqlite::params![id, want],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?)
                })
                .await
                .unwrap_or(None);
            let (conclusion, why) = match outcome.as_deref() {
                Some("passed") => ("success", format!("{test} passed in its run")),
                Some("failed" | "error") => ("failure", format!("{test} failed in its run")),
                Some(o) => ("untestable", format!("{test} was {o} in its run")),
                None => ("untestable", format!("its run ({c}) didn't report {test}")),
            };
            return Tried { sha: sha.to_owned(), run: Some(started), conclusion: conclusion.into(), why: Some(why) };
        }
        if std::time::Instant::now() >= deadline {
            return Tried { sha: sha.to_owned(), run: Some(started), conclusion: "untestable".into(), why: Some("its run took longer than three hours".into()) };
        }
        crate::rt::sleep(Duration::from_secs(2)).await;
    }
}

/// Starts culprit finding for a new failure of `check` in `run` (W28-R57), as kiln: a test's over
/// its unit's commits, a job's over every commit between.
///
/// Boxed: a bisection's end works its lane's verdicts out again, which may start another.
pub fn start_auto<'a>(hub: &'a Arc<Hub>, run: &'a str, check: &'a kiln_ci::verdict::Check) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let body = if check.test.is_empty() { json!({ "job": check.unit }) } else { json!({ "test": check.test }) };
        if let Err((_, why)) = start(hub, run, &body, "kiln (W28-R57)").await {
            tracing::info!(run = %run, check = %check, why = %why, "culprit finding didn't start");
        }
    })
}

/// When the server starts: bisections it was driving stopped with it. Each is said to have, to be
/// started again.
pub async fn interrupted(hub: &Hub) {
    for (_, t) in hub.store.kv_prefix("ci/bisect/".to_owned()).await.unwrap_or_default() {
        let Ok(mut b) = serde_json::from_str::<Bisect>(&t) else { continue };
        if b.state == "running" {
            b.state = "failed".into();
            b.why = Some("the server restarted while it ran: start it again".into());
            save(hub, &mut b).await;
        }
    }
}

/// `GET /api/ci/runs/:id/bisects[/:bisect]`.
pub async fn list(hub: &Hub, run: &str, one: Option<&str>) -> Result<Response, Fail> {
    let all = of_run(hub, run).await;
    match one {
        Some(id) => {
            all.into_iter().find(|b| b.id == id).map(|b| srv::json(StatusCode::OK, &json!({ "bisect": b }))).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no bisection `{id}` of run {run}")))
        }
        None => Ok(srv::json(StatusCode::OK, &json!({ "bisects": all }))),
    }
}
