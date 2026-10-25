//! CI providers (docs/ci.md W23): a repository's CI runs on KilnCI (this engine) or on its host's
//! own CI, which kiln drives through the host's API (GitHub Actions, GitLab CI/CD, Bitbucket
//! Pipelines), or on several. A provider's runs are kept in kiln's own tables, marked with the
//! provider and their ids there (W23-R09), so the run list, the run page, insights, notifications
//! and the API show every provider's runs alike; logs and artifacts are read from the provider when
//! asked for; reruns, cancels, dispatches and approvals go to it (R01).

pub mod bitbucket_pipelines;
pub mod github_actions;
pub mod gitlab_ci;

use std::collections::BTreeMap;
use std::sync::Arc;

use http::StatusCode;
use rusqlite::{OptionalExtension as _, params};
use serde::Serialize;
use serde_json::{Value, json};

use super::Fail;
use crate::authz::Caller;
use crate::hub::Hub;
use crate::store::Repo;

/// KilnCI: this engine.
pub const KILN: &str = "kiln";
pub const GITHUB_ACTIONS: &str = "github-actions";
pub const GITLAB_CI: &str = "gitlab-ci";
pub const BITBUCKET_PIPELINES: &str = "bitbucket-pipelines";

fn fail(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// What a provider said when it didn't do what was asked.
fn upstream(e: anyhow::Error) -> Fail {
    (StatusCode::BAD_GATEWAY, format!("{e:#}"))
}

/// The providers a repository's CI runs on (W23-R07): KilnCI unless its settings say otherwise.
pub async fn chosen(hub: &Hub, repo: &Repo) -> Vec<String> {
    let id = repo.id.clone();
    let saved: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT providers FROM repo_ci WHERE repo_id = ?1", [&id], |r| r.get(0)).optional()?)).await.ok().flatten();
    saved.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()).filter(|l| !l.is_empty()).unwrap_or_else(|| vec![KILN.to_owned()])
}

/// The providers a repository may have: KilnCI always, and its host's CI where kiln reaches it.
pub async fn available(hub: &Hub, repo: &Repo) -> Vec<&'static str> {
    let mut out = vec![KILN];
    if let Some(app) = crate::githost::github::app(hub).await
        && app.name_of(repo).is_some()
    {
        out.push(GITHUB_ACTIONS);
    }
    if crate::githost::gitlab::for_repo(hub, repo).await.is_some() {
        out.push(GITLAB_CI);
    }
    if crate::githost::bitbucket::for_repo(hub, repo).await.is_some() {
        out.push(BITBUCKET_PIPELINES);
    }
    out
}

/// Whether KilnCI runs a repository's workflows.
pub async fn kiln_runs(hub: &Hub, repo: &Repo) -> bool {
    chosen(hub, repo).await.iter().any(|p| p == KILN)
}

/// The other providers a repository's CI runs on.
async fn elsewhere(hub: &Hub, repo: &Repo) -> Vec<String> {
    chosen(hub, repo).await.into_iter().filter(|p| p != KILN).collect()
}

/// A run as a provider reports it, in kiln's words (W23-R09) and its own.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ExtRun {
    pub external_id: String,
    /// The provider's number for it.
    pub number: i64,
    /// The workflow's (pipeline's) name and file.
    pub name: String,
    pub path: String,
    pub title: Option<String>,
    pub event: String,
    pub branch: String,
    pub sha: String,
    pub actor: String,
    /// kiln's: `queued`, `running`, `succeeded`, `failed`, `cancelled`, `skipped`, `timed_out`.
    pub status: String,
    /// The provider's own (`success`, `failure`, …).
    pub conclusion: Option<String>,
    pub attempt: i64,
    pub url: String,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExtStep {
    pub number: i64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExtJob {
    pub external_id: String,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub runner: Option<String>,
    pub labels: Vec<String>,
    pub url: String,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub steps: Vec<ExtStep>,
}

/// Keeps a provider's run, and its jobs and steps when given, in kiln's tables: its kiln id. A run
/// already kept is brought up to date in place; a new one gets kiln's next number.
pub async fn record(hub: &Hub, repo: &Repo, provider: &str, run: &ExtRun, jobs: Option<&[ExtJob]>) -> anyhow::Result<String> {
    let (repo_id, provider, run, jobs) = (repo.id.clone(), provider.to_owned(), run.clone(), jobs.map(<[ExtJob]>::to_vec));
    let id = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let known: Option<String> = tx.query_row("SELECT id FROM runs WHERE repo_id = ?1 AND provider = ?2 AND external_id = ?3", params![repo_id, provider, run.external_id], |r| r.get(0)).optional()?;
            let id = match known {
                Some(id) => {
                    tx.execute(
                        "UPDATE runs SET status = ?2, conclusion = ?3, attempt = ?4, started_at = ?5, finished_at = ?6, run_name = COALESCE(?7, run_name), external_url = ?8 WHERE id = ?1",
                        params![id, run.status, run.conclusion, run.attempt, run.started_at, run.finished_at, run.title, run.url],
                    )?;
                    id
                }
                None => {
                    let id = crate::util::new_id();
                    let number: i64 = tx.query_row("SELECT COALESCE(MAX(number), 0) + 1 FROM runs WHERE repo_id = ?1", [&repo_id], |r| r.get(0))?;
                    tx.execute(
                        "INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, message, author, status, created_at, started_at, finished_at, workflow_id, actor, trigger, trust, priority,
                                           attempt, conclusion, run_name, provider, external_id, external_url, external_number)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '', ?8, ?9, ?10, ?11, ?12, ?13, ?8, '{}', 'trusted', 50, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
                        params![
                            id, repo_id, number, run.name, run.event, run.branch, run.sha, run.actor, run.status, run.created_at, run.started_at, run.finished_at, run.path, run.attempt, run.conclusion, run.title, provider,
                            run.external_id, run.url, run.number
                        ],
                    )?;
                    id
                }
            };
            let listed = jobs.as_ref().map(|l| l.iter().map(|j| j.external_id.clone()).collect::<Vec<_>>());
            if let Some(listed) = listed {
                // The provider lists the attempt's jobs as they are now: a retried job (GitLab's) is a
                // new one, and the one it replaced goes.
                let mut q = tx.prepare("SELECT id, external_id FROM jobs WHERE run_id = ?1 AND run_attempt = ?2")?;
                let kept: Vec<(String, Option<String>)> = q.query_map(params![id, run.attempt], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
                drop(q);
                for (leg, ext) in kept {
                    if !ext.is_some_and(|e| listed.contains(&e)) {
                        tx.execute("DELETE FROM steps WHERE job_id = ?1", [&leg])?;
                        tx.execute("DELETE FROM jobs WHERE id = ?1", [&leg])?;
                    }
                }
            }
            for j in jobs.unwrap_or_default() {
                let leg: Option<String> = tx.query_row("SELECT id FROM jobs WHERE run_id = ?1 AND external_id = ?2", params![id, j.external_id], |r| r.get(0)).optional()?;
                let labels = serde_json::to_string(&j.labels).unwrap_or_else(|_| "[]".into());
                let leg = match leg {
                    Some(l) => {
                        tx.execute(
                            "UPDATE jobs SET status = ?2, conclusion = ?3, agent = ?4, labels = ?5, started_at = ?6, finished_at = ?7, external_url = ?8, attempt = ?9 WHERE id = ?1",
                            params![l, j.status, j.conclusion, j.runner, labels, j.started_at, j.finished_at, j.url, run.attempt],
                        )?;
                        l
                    }
                    None => {
                        let l = crate::util::new_id();
                        tx.execute(
                            "INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, agent, executor, attempt, queued_at, started_at, finished_at, job_id, conclusion, external_id, external_url, run_attempt)
                             VALUES (?1, ?2, ?3, ?3, ?4, ?5, '[]', '{}', ?6, ?7, ?8, ?9, ?10, ?11, ?3, ?12, ?13, ?14, ?8)",
                            params![l, id, j.name, j.status, labels, j.runner, provider, run.attempt, run.created_at, j.started_at, j.finished_at, j.conclusion, j.external_id, j.url],
                        )?;
                        l
                    }
                };
                tx.execute("DELETE FROM steps WHERE job_id = ?1", [&leg])?;
                for st in &j.steps {
                    tx.execute(
                        "INSERT INTO steps (job_id, idx, name, status, started_at, finished_at, kind, outcome, conclusion, attempt) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'external', ?7, ?7, ?8)",
                        params![leg, st.number.max(1) - 1, st.name, st.status, st.started_at, st.finished_at, st.conclusion, run.attempt],
                    )?;
                }
            }
            tx.commit()?;
            Ok(id)
        })
        .await?;
    Ok(id)
}

/// A run's provider and its id there, if it isn't KilnCI's.
pub async fn of_run(hub: &Hub, run: &str) -> anyhow::Result<Option<(Repo, String, String)>> {
    let key = run.to_owned();
    let row: Option<(String, String, Option<String>)> = hub.store.call(move |c| Ok(c.query_row("SELECT repo_id, provider, external_id FROM runs WHERE id = ?1", [&key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?)).await?;
    let Some((repo_id, provider, Some(ext))) = row.filter(|(_, p, _)| p != KILN) else { return Ok(None) };
    let repo = hub.store.repos().await?.into_iter().find(|r| r.id == repo_id).ok_or_else(|| anyhow::anyhow!("the run's repository is gone"))?;
    Ok(Some((repo, provider, ext)))
}

/// A leg's provider and the ids it has there, if it isn't KilnCI's: (repository, provider, run, job).
pub async fn of_leg(hub: &Hub, leg: &str) -> anyhow::Result<Option<(Repo, String, String, String)>> {
    let key = leg.to_owned();
    let row: Option<(String, String, Option<String>, Option<String>)> = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT r.repo_id, r.provider, r.external_id, j.external_id FROM jobs j JOIN runs r ON r.id = j.run_id WHERE j.id = ?1", [&key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?))
        .await?;
    let Some((repo_id, provider, Some(run), Some(job))) = row.filter(|(_, p, _, _)| p != KILN) else { return Ok(None) };
    let repo = hub.store.repos().await?.into_iter().find(|r| r.id == repo_id).ok_or_else(|| anyhow::anyhow!("the run's repository is gone"))?;
    Ok(Some((repo, provider, run, job)))
}

/// Reruns a provider's run: `all`, `failed`, or one `job` (the leg's kiln id); GitLab's `play`.
pub async fn rerun(hub: &Hub, repo: &Repo, provider: &str, run: &str, mode: &str, job: Option<&str>) -> Result<Value, Fail> {
    match provider {
        GITHUB_ACTIONS => github_actions::rerun(hub, repo, run, mode, job).await.map_err(upstream),
        GITLAB_CI => gitlab_ci::rerun(hub, repo, run, mode, job).await.map_err(upstream),
        BITBUCKET_PIPELINES => bitbucket_pipelines::rerun(hub, repo, run, mode).await.map_err(upstream),
        other => Err((StatusCode::NOT_IMPLEMENTED, format!("{other} doesn't rerun from kiln"))),
    }
}

pub async fn cancel(hub: &Hub, repo: &Repo, provider: &str, run: &str) -> Result<Value, Fail> {
    match provider {
        GITHUB_ACTIONS => github_actions::cancel(hub, repo, run).await.map_err(upstream),
        GITLAB_CI => gitlab_ci::cancel(hub, repo, run).await.map_err(upstream),
        BITBUCKET_PIPELINES => bitbucket_pipelines::cancel(hub, repo, run).await.map_err(upstream),
        other => Err((StatusCode::NOT_IMPLEMENTED, format!("{other} doesn't cancel from kiln"))),
    }
}

/// A provider's leg's log, as text: the leg is `job` of the provider's run `run`.
pub async fn logs(hub: &Hub, repo: &Repo, provider: &str, run: &str, job: &str) -> Result<String, Fail> {
    match provider {
        GITHUB_ACTIONS => github_actions::logs(hub, repo, job).await.map_err(upstream),
        GITLAB_CI => gitlab_ci::logs(hub, repo, job).await.map_err(upstream),
        BITBUCKET_PIPELINES => bitbucket_pipelines::logs(hub, repo, run, job).await.map_err(upstream),
        other => Err((StatusCode::NOT_IMPLEMENTED, format!("{other}'s logs aren't read here"))),
    }
}

/// A manual run of a repository whose CI isn't KilnCI's (`None` when KilnCI's is chosen, so KilnCI
/// runs it): `workflow` (GitHub's file, Bitbucket's custom pipeline; GitLab has one) at `git_ref`
/// with `inputs` (GitLab's and Bitbucket's variables).
pub async fn dispatch(hub: &Hub, repo: &Repo, workflow: &str, git_ref: &str, inputs: &Value) -> Option<Result<(), Fail>> {
    let chosen = chosen(hub, repo).await;
    if chosen.iter().any(|p| p == KILN) {
        return None;
    }
    let provider = chosen.into_iter().next()?;
    Some(match provider.as_str() {
        GITHUB_ACTIONS => github_actions::dispatch(hub, repo, workflow, git_ref, inputs).await.map_err(upstream),
        GITLAB_CI => gitlab_ci::dispatch(hub, repo, git_ref, inputs).await.map(|_| ()).map_err(upstream),
        BITBUCKET_PIPELINES => bitbucket_pipelines::dispatch(hub, repo, workflow, git_ref, inputs).await.map(|_| ()).map_err(upstream),
        other => Err((StatusCode::NOT_IMPLEMENTED, format!("{other} doesn't start runs from kiln"))),
    })
}

/// A run's artifacts on its provider: `[{ id, name, size, created_at, expires_at, expired }]`.
pub async fn artifacts(hub: &Hub, repo: &Repo, provider: &str, run: &str) -> Result<Vec<Value>, Fail> {
    match provider {
        GITHUB_ACTIONS => github_actions::artifacts(hub, repo, run).await.map_err(upstream),
        GITLAB_CI => gitlab_ci::artifacts(hub, repo, run).await.map_err(upstream),
        // Bitbucket's API doesn't serve a pipeline's artifacts.
        _ => Ok(Vec::new()),
    }
}

/// One artifact's archive.
pub async fn artifact(hub: &Hub, repo: &Repo, provider: &str, run: &str, artifact: &str) -> Result<bytes::Bytes, Fail> {
    let _ = run;
    match provider {
        GITHUB_ACTIONS => github_actions::artifact(hub, repo, artifact).await.map_err(upstream),
        GITLAB_CI => gitlab_ci::artifact(hub, repo, artifact).await.map_err(upstream),
        other => Err((StatusCode::NOT_IMPLEMENTED, format!("{other}'s API doesn't serve artifacts"))),
    }
}

/// What a run waits for on its provider: deployments to review.
pub async fn pending(hub: &Hub, repo: &Repo, provider: &str, run: &str) -> Result<Vec<Value>, Fail> {
    match provider {
        GITHUB_ACTIONS => github_actions::pending(hub, repo, run).await.map_err(upstream),
        GITLAB_CI => gitlab_ci::pending(hub, repo, run).await.map_err(upstream),
        _ => Ok(Vec::new()),
    }
}

/// Approves or rejects what a run waits for on its provider, as the person when they signed in with
/// that host (W23-R08), else with kiln's credentials there.
#[allow(clippy::too_many_arguments)]
pub async fn approve(hub: &Hub, repo: &Repo, provider: &str, run: &str, environments: &[String], approved: bool, comment: &str, caller: Option<&Caller>) -> Result<Value, Fail> {
    let user = caller.and_then(|c| c.user.as_deref());
    match provider {
        GITHUB_ACTIONS => {
            let web = crate::githost::github::app(hub).await.map(|a| a.web.clone()).unwrap_or_default();
            let token = match user {
                Some(u) => crate::authz::sso::person_token(hub, "github", u, &web).await,
                None => None,
            };
            github_actions::approve(hub, repo, run, environments, approved, comment, token.as_deref()).await.map_err(upstream)
        }
        GITLAB_CI => {
            let web = crate::githost::gitlab::for_repo(hub, repo).await.map(|(g, _)| g.web.clone()).unwrap_or_default();
            let token = match user {
                Some(u) => crate::authz::sso::person_token(hub, "gitlab", u, &web).await,
                None => None,
            };
            gitlab_ci::approve(hub, repo, run, environments, approved, comment, token.as_deref()).await.map_err(upstream)
        }
        other => Err((StatusCode::NOT_IMPLEMENTED, format!("{other} has no deployment approvals kiln can give"))),
    }
}

/// The repository's environments on its other providers, each with its latest deployment.
pub async fn environments(hub: &Hub, repo: &Repo) -> Vec<Value> {
    let mut out = Vec::new();
    for p in elsewhere(hub, repo).await {
        let got = match p.as_str() {
            GITHUB_ACTIONS => github_actions::environments(hub, repo).await,
            GITLAB_CI => gitlab_ci::environments(hub, repo).await,
            BITBUCKET_PIPELINES => bitbucket_pipelines::environments(hub, repo).await,
            _ => Ok(Vec::new()),
        };
        match got {
            Ok(l) => out.extend(l),
            Err(e) => out.push(json!({ "provider": p, "error": format!("{e:#}") })),
        }
    }
    out
}

/// `GET /api/ci/repos/:repo/providers/workflows`: each chosen provider's CI files, checked: GitHub's
/// YAML by kiln's own checks (W02), GitLab's by its CI lint; Bitbucket's API has no check.
pub async fn workflows(hub: &Hub, repo_key: &str) -> Result<Value, Fail> {
    let repo = repo_by(hub, repo_key).await?;
    let mut out = Vec::new();
    for p in chosen(hub, &repo).await {
        out.push(match p.as_str() {
            GITLAB_CI => gitlab_ci::lint(hub, &repo, None).await.unwrap_or_else(|e| json!({ "provider": p, "path": ".gitlab-ci.yml", "error": format!("{e:#}") })),
            BITBUCKET_PIPELINES => json!({ "provider": p, "path": "bitbucket-pipelines.yml", "checked": false, "note": "Bitbucket's API has no check of bitbucket-pipelines.yml" }),
            // KilnCI and GitHub Actions read the same files, checked as W02 checks them.
            _ => json!({ "provider": p, "path": ".github/workflows", "checked_at": format!("/api/ci/repos/{}/workflows", crate::ci::cli::enc(&repo.name)) }),
        });
    }
    Ok(json!({ "repo": repo.name, "workflows": out }))
}

/// `GET /api/ci/repos/:repo/commits/:sha/checks`: every provider's runs of a commit, together.
pub async fn checks(hub: &Hub, repo_key: &str, sha: &str) -> Result<Value, Fail> {
    let repo = repo_by(hub, repo_key).await?;
    let (id, want) = (repo.id.clone(), sha.to_ascii_lowercase());
    let runs: Vec<Value> = hub
        .store
        .call(move |c| {
            let mut q = c.prepare("SELECT id, provider, COALESCE(run_name, pipeline), status, conclusion, external_url, number, created_at, finished_at FROM runs WHERE repo_id = ?1 AND (sha = ?2 OR sha LIKE ?2 || '%') ORDER BY created_at")?;
            let rows = q.query_map(params![id, want], |r| {
                Ok(json!({ "run": r.get::<_, String>(0)?, "provider": r.get::<_, String>(1)?, "name": r.get::<_, Option<String>>(2)?, "status": r.get::<_, String>(3)?, "conclusion": r.get::<_, Option<String>>(4)?,
                           "url": r.get::<_, Option<String>>(5)?, "number": r.get::<_, i64>(6)?, "created_at": r.get::<_, i64>(7)?, "finished_at": r.get::<_, Option<i64>>(8)? }))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(fail)?;
    let state = if runs.iter().any(|r| matches!(r.get("status").and_then(Value::as_str), Some("failed" | "timed_out"))) {
        "failure"
    } else if runs.iter().any(|r| matches!(r.get("status").and_then(Value::as_str), Some("queued" | "running"))) {
        "pending"
    } else if runs.is_empty() {
        "none"
    } else {
        "success"
    };
    Ok(json!({ "repo": repo.name, "sha": sha, "state": state, "checks": runs }))
}

/// `POST /api/ci/repos/:repo/providers/sync`: the repository's runs on its other providers, looked
/// at now.
pub async fn sync_now(hub: &Hub, repo_key: &str) -> Result<Value, Fail> {
    let repo = repo_by(hub, repo_key).await?;
    let mut done = Vec::new();
    for p in elsewhere(hub, &repo).await {
        sync_one(hub, &repo, &p).await.map_err(upstream)?;
        done.push(p);
    }
    Ok(json!({ "repo": repo.name, "synced": done }))
}

async fn sync_one(hub: &Hub, repo: &Repo, provider: &str) -> anyhow::Result<()> {
    match provider {
        GITHUB_ACTIONS => github_actions::sync(hub, repo).await,
        GITLAB_CI => gitlab_ci::sync(hub, repo).await,
        BITBUCKET_PIPELINES => bitbucket_pipelines::sync(hub, repo).await,
        _ => Ok(()),
    }
}

/// `GET /api/ci/providers/:repo`: what the repository's CI runs on, and what it may.
pub async fn show(hub: &Hub, repo_key: &str) -> Result<Value, Fail> {
    let repo = repo_by(hub, repo_key).await?;
    Ok(json!({ "repo": repo.name, "providers": chosen(hub, &repo).await, "available": available(hub, &repo).await }))
}

/// `PUT /api/ci/providers/:repo` `{ providers: [...] }`.
pub async fn set(hub: &Hub, repo_key: &str, body: &Value, by: &str) -> Result<Value, Fail> {
    let repo = repo_by(hub, repo_key).await?;
    let asked: Vec<String> = body.get("providers").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    if asked.is_empty() {
        return Err((StatusCode::BAD_REQUEST, format!("`providers`: at least one ({KILN}, {GITHUB_ACTIONS}, {GITLAB_CI}, {BITBUCKET_PIPELINES})")));
    }
    let may = available(hub, &repo).await;
    if let Some(p) = asked.iter().find(|p| !may.contains(&p.as_str())) {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("{p} isn't one {} may have here: {}", repo.name, may.join(", "))));
    }
    let (id, list) = (repo.id.clone(), serde_json::to_string(&asked).unwrap_or_default());
    hub.store
        .call(move |c| {
            c.execute("INSERT INTO repo_ci (repo_id, providers, updated_at) VALUES (?1, ?2, ?3) ON CONFLICT(repo_id) DO UPDATE SET providers = excluded.providers, updated_at = excluded.updated_at", params![id, list, crate::util::now_unix()])?;
            Ok(())
        })
        .await
        .map_err(fail)?;
    hub.store.audit(by.to_owned(), "ci.providers.set".into(), format!("{}: {}", repo.name, asked.join(", "))).await;
    Ok(json!({ "repo": repo.name, "providers": asked }))
}

async fn repo_by(hub: &Hub, key: &str) -> Result<Repo, Fail> {
    hub.store.repos().await.map_err(fail)?.into_iter().find(|r| r.name == key || r.id == key).ok_or((StatusCode::NOT_FOUND, "no such repository".into()))
}

/// Keeps every external provider's runs in kiln's tables, for as long as the server runs: what
/// changed since the last look, every 20 seconds (webhooks bring a run sooner).
pub async fn sync_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    loop {
        if !hub.shutdown.sleep(std::time::Duration::from_secs(20)).await {
            return Ok(());
        }
        let Ok(repos) = hub.store.repos().await else { continue };
        for repo in repos {
            for p in elsewhere(&hub, &repo).await {
                if let Err(e) = sync_one(&hub, &repo, &p).await {
                    tracing::debug!(repo = %repo.name, provider = %p, error = %format!("{e:#}"), "a provider's runs didn't sync");
                }
            }
        }
    }
}

/// kiln's state for a provider's status and conclusion (GitHub's words, which GitLab's and
/// Bitbucket's are mapped to first).
pub fn kiln_status(status: &str, conclusion: Option<&str>) -> &'static str {
    match status {
        "queued" | "requested" | "pending" | "waiting" | "created" | "preparing" | "scheduled" | "manual" => "queued",
        "in_progress" | "running" => "running",
        _ => match conclusion.unwrap_or_default() {
            "success" | "neutral" => "succeeded",
            "cancelled" | "canceled" | "stale" => "cancelled",
            "skipped" => "skipped",
            "timed_out" => "timed_out",
            _ => "failed",
        },
    }
}

/// An RFC 3339 time as Unix seconds.
pub fn unix(v: Option<&Value>) -> Option<i64> {
    let s = v?.as_str()?;
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok().map(time::OffsetDateTime::unix_timestamp)
}

/// The settings kiln keeps per repository and provider (unused fields are free for providers).
pub type Settings = BTreeMap<String, Value>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w23_r09_states() {
        assert_eq!(kiln_status("queued", None), "queued");
        assert_eq!(kiln_status("in_progress", None), "running");
        assert_eq!(kiln_status("completed", Some("success")), "succeeded");
        assert_eq!(kiln_status("completed", Some("failure")), "failed");
        assert_eq!(kiln_status("completed", Some("cancelled")), "cancelled");
        assert_eq!(kiln_status("completed", Some("timed_out")), "timed_out");
        assert_eq!(unix(Some(&json!("2026-10-18T12:00:00Z"))), Some(1_792_324_800));
    }
}
