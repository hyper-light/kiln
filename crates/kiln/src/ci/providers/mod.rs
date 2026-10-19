//! CI providers (docs/ci.md W23): a repository's CI runs on KilnCI (this engine) or on its host's
//! own CI, which kiln drives through the host's API (GitHub Actions here; GitLab CI/CD and Bitbucket
//! Pipelines with their hosts), or on several. A provider's runs are kept in kiln's own tables,
//! marked with the provider and their ids there (W23-R09), so the run list, the run page, insights,
//! notifications and the API show every provider's runs alike; logs are read from the provider when
//! asked for; reruns, cancels, dispatches and approvals go to it.

pub mod github_actions;

use std::collections::BTreeMap;
use std::sync::Arc;

use http::StatusCode;
use rusqlite::{OptionalExtension as _, params};
use serde::Serialize;
use serde_json::{Value, json};

use super::Fail;
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
    out
}

/// Whether KilnCI runs a repository's workflows.
pub async fn kiln_runs(hub: &Hub, repo: &Repo) -> bool {
    chosen(hub, repo).await.iter().any(|p| p == KILN)
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
            let tx = c.transaction()?;
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

/// Reruns a provider's run: `all`, `failed`, or one `job` (the leg's kiln id).
pub async fn rerun(hub: &Hub, repo: &Repo, provider: &str, run: &str, mode: &str, job: Option<&str>) -> Result<Value, Fail> {
    match provider {
        GITHUB_ACTIONS => github_actions::rerun(hub, repo, run, mode, job).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}"))),
        other => Err((StatusCode::NOT_IMPLEMENTED, format!("{other} doesn't rerun from kiln yet"))),
    }
}

pub async fn cancel(hub: &Hub, repo: &Repo, provider: &str, run: &str) -> Result<Value, Fail> {
    match provider {
        GITHUB_ACTIONS => github_actions::cancel(hub, repo, run).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}"))),
        other => Err((StatusCode::NOT_IMPLEMENTED, format!("{other} doesn't cancel from kiln yet"))),
    }
}

/// A provider's leg's log, as text.
pub async fn logs(hub: &Hub, repo: &Repo, provider: &str, job: &str) -> Result<String, Fail> {
    match provider {
        GITHUB_ACTIONS => github_actions::logs(hub, repo, job).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}"))),
        other => Err((StatusCode::NOT_IMPLEMENTED, format!("{other}'s logs come later"))),
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
        return Err((StatusCode::BAD_REQUEST, "`providers`: at least one (`kiln`, `github-actions`)".into()));
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
            let chosen = chosen(&hub, &repo).await;
            if chosen.iter().any(|p| p == GITHUB_ACTIONS)
                && let Err(e) = github_actions::sync(&hub, &repo).await
            {
                tracing::debug!(repo = %repo.name, error = %format!("{e:#}"), "GitHub Actions' runs didn't sync");
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
