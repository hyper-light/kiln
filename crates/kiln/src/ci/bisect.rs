//! Bisecting a failure (docs/ci.md §4.1's `ci.runs.bisect`, W20-R09's `diagnose`;
//! docs/ci-agents.md): a failed run's job, run again on the commits between the last one it passed
//! on and the run's own, the range halved each time, until the first commit it fails on is found.
//! Each try is an ordinary run of that job (and the jobs it needs) at that commit, on the run's
//! branch, trusted as the failed run was: the commits are its own history. A try that can't run
//! (it didn't start, it was cancelled) ends the bisection inconclusive, saying which and why.

use std::sync::Arc;
use std::time::Duration;

use http::StatusCode;
use rusqlite::OptionalExtension as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
    pub tried: Vec<Tried>,
    /// `running`, `found` (`culprit`), `inconclusive` or `failed` (`why`).
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
    let job = match (asked, failed.as_slice()) {
        (Some(j), _) => j,
        (None, [one]) => one.clone(),
        (None, []) => return Err(conflict("none of the run's jobs failed: say which to bisect (`job`)")),
        (None, many) => return Err(conflict(format!("{} failed: say which to bisect (`job`)", many.join(", ")))),
    };
    let bad = text("bad").unwrap_or_else(|| f.sha.clone());
    let good = match text("good") {
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
    let out = git(vec!["-C".into(), mirror, "rev-list".into(), "--reverse".into(), "--first-parent".into(), "--ancestry-path".into(), format!("{good}..{bad}")]).await.map_err(internal)?;
    let commits: Vec<String> = String::from_utf8_lossy(&out).lines().map(str::trim).filter(|l| is_sha(l)).map(str::to_owned).collect();
    if commits.is_empty() {
        return Err(conflict("there's nothing between them to bisect"));
    }
    if commits.len() > MOST {
        return Err(conflict(format!("{} commits are more than a bisection takes on ({MOST}): say a later `good`", commits.len())));
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
        state: "running".into(),
        by: by.to_owned(),
        created_at: now,
        ..Bisect::default()
    };
    save(hub, &mut b).await;
    hub.store.audit(by.to_owned(), "ci.runs.bisect".into(), format!("{} run #{}: {} between {} and {} ({} commits)", f.repo, f.number, b.job, b.good, b.bad, b.commits.len())).await;
    let answer = b.clone();
    let hub = Arc::clone(hub);
    crate::rt::spawn(async move { drive(hub, b, f.trusted).await });
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "bisect": answer })))
}

/// Halves `b`'s range until one commit is left: the first the job fails on.
async fn drive(hub: Arc<Hub>, mut b: Bisect, trusted: bool) {
    let Some(repo) = hub.store.repos().await.ok().and_then(|r| r.into_iter().find(|r| r.name == b.repo)) else {
        b.state = "failed".into();
        b.why = Some(format!("{} is gone", b.repo));
        save(&hub, &mut b).await;
        return;
    };
    // The first failing commit is in commits[lo..=hi]; the last one fails (it's the run's).
    let (mut lo, mut hi) = (0usize, b.commits.len().saturating_sub(1));
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let Some(sha) = b.commits.get(mid).cloned() else { break };
        let t = try_commit(&hub, &repo, &b, &sha, trusted).await;
        let passed = t.conclusion == "success";
        let failed = matches!(t.conclusion.as_str(), "failure" | "timed_out");
        b.tried.push(t.clone());
        if passed {
            lo = mid + 1;
        } else if failed {
            hi = mid;
        } else {
            b.state = "inconclusive".into();
            b.why = Some(format!("{} couldn't be tested: {}", sha.get(..12).unwrap_or(&sha), t.why.unwrap_or_else(|| t.conclusion.clone())));
            save(&hub, &mut b).await;
            return;
        }
        save(&hub, &mut b).await;
    }
    let sha = b.commits.get(lo).cloned().unwrap_or_else(|| b.bad.clone());
    let (message, author) = crate::gitops::commit_info(&hub, &repo, &sha).await;
    b.culprit = Some(Culprit { sha, message, author });
    b.state = "found".into();
    save(&hub, &mut b).await;
}

/// The job run at `sha`, to its end.
async fn try_commit(hub: &Arc<Hub>, repo: &crate::store::Repo, b: &Bisect, sha: &str, trusted: bool) -> Tried {
    let ask =
        super::uploads::Ask { sha: sha.to_owned(), git_ref: b.branch.clone(), event: "push".into(), workflows: vec![b.workflow.clone()], job: Some(b.job.clone()), ..super::uploads::Ask::default() };
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
            return Tried { sha: sha.to_owned(), run: Some(started), conclusion: c, why: None };
        }
        if std::time::Instant::now() >= deadline {
            return Tried { sha: sha.to_owned(), run: Some(started), conclusion: "untestable".into(), why: Some("its run took longer than three hours".into()) };
        }
        crate::rt::sleep(Duration::from_secs(2)).await;
    }
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
