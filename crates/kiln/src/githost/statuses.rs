//! KilnCI's runs of repositories on GitLab or Bitbucket, reported there as commit statuses (docs/ci.md
//! W23; GitHub's runs get check runs, [`super::github::report`]): one status a run, named for it
//! (`kiln/ci`), following it as it moves and linking to it on kiln; caught up every minute where
//! the host didn't take one. A provider's own run (GitLab CI's pipeline) is the host's already.

use std::sync::Arc;
use std::time::Duration;

use rusqlite::OptionalExtension as _;
use serde_json::Value;

use super::{CommitStatus, GitHost};
use crate::hub::Hub;

/// A run's state as a commit status (GitHub's words; each host's own from them).
fn state(status: &str, conclusion: Option<&str>) -> &'static str {
    match status {
        "pending" | "queued" | "waiting" => "pending",
        "running" => "running",
        _ => match conclusion.unwrap_or(status) {
            "success" | "succeeded" | "skipped" | "neutral" => "success",
            "cancelled" | "canceled" => "cancelled",
            _ => "failure",
        },
    }
}

fn settled(state: &str) -> bool {
    matches!(state, "success" | "failure" | "cancelled")
}

/// Brings the host's commit status for KilnCI's run `run_id` to where the run is, where its
/// repository is on a GitLab or Bitbucket kiln reaches.
pub async fn report(hub: &Hub, run_id: &str) -> anyhow::Result<()> {
    if hub.cfg.ci.gitlab.is_empty() && super::bitbucket::host(hub).await.is_none() {
        return Ok(());
    }
    let id = run_id.to_owned();
    #[allow(clippy::type_complexity)]
    let row: Option<(String, String, String, String, Option<String>, String, i64)> = hub
        .store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT repo_id, sha, COALESCE(run_name, pipeline), status, conclusion, provider, attempt FROM runs WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .optional()?)
        })
        .await?;
    let Some((repo_id, sha, name, status, conclusion, provider, attempt)) = row else { return Ok(()) };
    if provider != "kiln" || sha.is_empty() {
        return Ok(());
    }
    let repos = hub.store.repos().await?;
    let Some(repo) = repos.into_iter().find(|r| r.id == repo_id) else { return Ok(()) };
    let host = match super::for_repo(hub, &repo).await {
        Some(h @ (GitHost::Gitlab(_) | GitHost::Bitbucket(_))) => h,
        _ => return Ok(()),
    };
    let want = state(&status, conclusion.as_deref());
    let key = format!("ci/host-status/{run_id}");
    let mark = format!("{want}@{attempt}");
    if hub.store.kv_get(key.clone()).await?.as_deref() == Some(mark.as_str()) {
        return Ok(());
    }
    let said = match want {
        "pending" => "waiting on kiln",
        "running" => "running on kiln",
        "success" => "passed on kiln",
        "cancelled" => "cancelled on kiln",
        _ => "failed on kiln",
    };
    let page = format!("{}/runs/{run_id}", hub.cfg.server.public_url.trim_end_matches('/'));
    let s = CommitStatus { context: format!("kiln/{name}"), state: want.to_owned(), description: format!("{name}: {said}"), url: Some(page) };
    host.status(hub, &repo, &sha, &s).await?;
    hub.store.kv_set(key, mark).await?;
    Ok(())
}

/// Keeps GitLab's and Bitbucket's commit statuses with KilnCI's runs, for as long as the server
/// runs: each run's change as it's broadcast, and every minute the ones a host didn't take.
pub async fn statuses_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    let mut rx = hub.events.subscribe();
    loop {
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            m = rx.recv().fuse() => match m {
                Ok(text) => {
                    if let Some(id) = run_of_event(&text)
                        && let Err(e) = report(&hub, &id).await
                    {
                        tracing::debug!(run = %id, error = %format!("{e:#}"), "a host didn't take a run's commit status yet");
                        let _ = hub.store.kv_set(format!("ci/host-status/{id}"), "unreported".into()).await;
                    }
                }
                Err(async_broadcast::RecvError::Overflowed(_)) => reconcile(&hub).await,
                Err(async_broadcast::RecvError::Closed) => return Ok(()),
            },
            () = crate::rt::sleep(Duration::from_secs(60)).fuse() => reconcile(&hub).await,
        }
    }
}

fn run_of_event(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    (v.get("type").and_then(Value::as_str) == Some("run")).then(|| v.pointer("/run/id").and_then(Value::as_str).map(str::to_owned)).flatten()
}

/// The runs whose status a host hasn't taken as they are, reported again.
async fn reconcile(hub: &Hub) {
    let Ok(marks) = hub.store.kv_prefix("ci/host-status/".into()).await else { return };
    for (key, mark) in marks.into_iter().filter(|(_, m)| !settled(m.split('@').next().unwrap_or_default())).take(200) {
        let Some(id) = key.strip_prefix("ci/host-status/") else { continue };
        if let Err(e) = report(hub, id).await {
            tracing::debug!(run = %id, mark = %mark, error = %format!("{e:#}"), "a run's commit status's catch-up");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_s_state_as_a_commit_status() {
        assert_eq!(state("queued", None), "pending");
        assert_eq!(state("running", None), "running");
        assert_eq!(state("succeeded", Some("success")), "success");
        assert_eq!(state("failed", Some("failure")), "failure");
        assert_eq!(state("cancelled", None), "cancelled");
        assert!(settled("failure") && !settled("running"));
    }
}
