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
    let mut rx = hub.subscribe_events()?;
    // Every minute, whatever else comes: a sleep made again on every pass never ended while events kept coming, and the
    // catch-up never ran then (goal 9, plan 01a1a87b2cec step 3). Its first tick is at once: taken here.
    let mut minute = crate::rt::Interval::new(RECONCILE_EVERY);
    minute.tick().await;
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
            () = minute.tick().fuse() => reconcile(&hub).await,
        }
    }
}

fn run_of_event(text: &str) -> Option<String> {
    // A run's events alone are parsed: the rest, every agent session's change of its info among them,
    // are passed over by their front (goal 4, plan 01a1a59c2a9c step 5).
    if !crate::wire::events::is_one_of(text, &["run"]) {
        return None;
    }
    let v: Value = serde_json::from_str(text).ok()?;
    (v.get("type").and_then(Value::as_str) == Some("run")).then(|| v.pointer("/run/id").and_then(Value::as_str).map(str::to_owned)).flatten()
}

/// The runs whose status a host hasn't taken as they are, reported again.
async fn reconcile(hub: &Hub) {
    let Ok(marks) = unsettled(hub).await else { return };
    for (key, mark) in marks {
        let Some(id) = key.strip_prefix("ci/host-status/") else { continue };
        if let Err(e) = report(hub, id).await {
            tracing::debug!(run = %id, mark = %mark, error = %format!("{e:#}"), "a run's commit status's catch-up");
        }
    }
}

/// How often the runs a host didn't take are reported again.
const RECONCILE_EVERY: Duration = Duration::from_secs(60);

/// The most of them reported again at once.
const RECONCILE_MOST: usize = 200;

/// The marks of the runs whose status a host hasn't taken as they are (not settled), [`RECONCILE_MOST`] at the most, in
/// their keys' order: read and picked out on the store's side. Every mark kept was read, then looked through and let go
/// of on this task's shard each minute, one a run for as long as kiln has run them (goal 9, plan 01a1a87b2cec step 3).
async fn unsettled(hub: &Hub) -> anyhow::Result<Vec<(String, String)>> {
    hub.store
        .read(|c| {
            let mut st = c.prepare("SELECT k, v FROM kv WHERE substr(k, 1, length(?1)) = ?1 ORDER BY k")?;
            let rows = st.query_map(["ci/host-status/"], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut out = Vec::new();
            for row in rows {
                let (k, v) = row?;
                if !settled(v.split('@').next().unwrap_or_default()) {
                    out.push((k, v));
                    if out.len() >= RECONCILE_MOST {
                        break;
                    }
                }
            }
            Ok(out)
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 9, plan 01a1a87b2cec step 3: the runs a host hasn't taken are picked out on the store's side, settled ones left
    /// there, in their keys' order and no more than the most reported again at once.
    #[test]
    fn g9_s3_the_unsettled_runs_are_picked_out_where_theyre_kept() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            for (run, mark) in [("r1", "success@1"), ("r2", "running@1"), ("r3", "unreported"), ("r4", "failure@2"), ("r5", "pending@1")] {
                hub.store.kv_set(format!("ci/host-status/{run}"), mark.into()).await?;
            }
            let picked: Vec<String> = unsettled(&hub).await?.into_iter().map(|(k, _)| k).collect();
            anyhow::ensure!(picked == ["ci/host-status/r2", "ci/host-status/r3", "ci/host-status/r5"], "{picked:?}");
            for n in 0..RECONCILE_MOST + 5 {
                hub.store.kv_set(format!("ci/host-status/s{n:04}"), "running@1".into()).await?;
            }
            anyhow::ensure!(unsettled(&hub).await?.len() == RECONCILE_MOST, "no more than the most at once");
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: a run's event says its run; any other, an agent session's info
    /// change among them, is passed over by its front.
    #[test]
    fn g4_s5_only_a_runs_event_is_read_for_its_run() -> anyhow::Result<()> {
        anyhow::ensure!(run_of_event(r#"{"type":"run","run":{"id":"r1","status":"running"}}"#).as_deref() == Some("r1"));
        anyhow::ensure!(run_of_event(crate::wire::events::harness_frame(&serde_json::to_vec(&crate::harness::session::Info { id: "s".into(), kind: "claude".into(), ..Default::default() })?).as_ref()).is_none() && run_of_event(r#"{"type":"run.failed","run":{"id":"r1"}}"#).is_none());
        anyhow::ensure!(run_of_event(r#"{ "type": "run", "run": {"id": "r2"} }"#).as_deref() == Some("r2"), "a frame whose kind isn't at its front, read whole");
        Ok(())
    }

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
