//! GitHub's webhooks for the CI engine (docs/ci.md W03-R02, W03-R07, W03-R14). A delivery's
//! payload becomes `github.event` as GitHub sent it.
//!
//! - **Moves polling also sees** (a push, a pull request opened or synchronized): the ref is
//!   recorded first, with the same compare-and-set polls use, so the move triggers once, whichever
//!   source saw it first. The usual path then runs, `.kiln/` pipelines included.
//! - **Events polling can't see** (comments, reviews, labels, releases, …): they start the
//!   matching workflows of the commit GitHub's table gives.
//! - **Redeliveries:** a delivery id (`X-GitHub-Delivery`) seen in the last 24 hours does nothing.

use std::collections::HashMap;
use std::sync::Arc;

use kiln_ci::trigger::github;
use rusqlite::params;
use serde_json::Value;

use super::{events, load};
use crate::gitops::{self, Trigger};
use crate::hub::Hub;

/// A delivery, after its signature checked out.
pub async fn github(hub: &Arc<Hub>, name: &str, delivery: Option<String>, body: &[u8]) -> anyhow::Result<()> {
    if !hub.ci.enabled() {
        // Without the engine a webhook only hints that a poll is due, as before.
        hub.gitops_kick.notify();
        return Ok(());
    }
    if let Some(id) = delivery
        && !first_delivery(hub, &id, name).await?
    {
        tracing::debug!(delivery = %id, "webhook redelivered; ignored");
        return Ok(());
    }
    let payload: Value = serde_json::from_slice(body)?;
    let full = payload.pointer("/repository/full_name").and_then(Value::as_str).unwrap_or_default().to_owned();
    let Some(repo) = hub.store.repos().await?.into_iter().find(|r| r.name.eq_ignore_ascii_case(&full)) else {
        hub.gitops_kick.notify();
        return Ok(());
    };
    // A move a poll would also see.
    if let Some((r, after)) = github::transition(name, &payload) {
        let tracked = match r.strip_prefix("refs/heads/") {
            Some(b) => repo.branches.is_empty() || repo.branches.iter().any(|p| crate::pipeline::glob_match(p, b)),
            None => r.starts_with("refs/tags/") || (r.starts_with("refs/pull/") && hub.cfg.gitops.pull_requests),
        };
        if tracked {
            gitops::ensure_commit(hub, &repo, &after).await?;
            if !hub.store.advance_ref(repo.id.clone(), r.clone(), after.clone()).await? {
                tracing::debug!(repo = %repo.name, r#ref = %r, "a poll saw this move first");
                return Ok(());
            }
            let (event, branch) = if let Some(b) = r.strip_prefix("refs/heads/") {
                ("push", b.to_owned())
            } else if let Some(t) = r.strip_prefix("refs/tags/") {
                ("tag", t.to_owned())
            } else {
                ("pull_request", format!("pr/{}", r.trim_start_matches("refs/pull/").trim_end_matches("/head")))
            };
            let before = payload.pointer("/before").and_then(Value::as_str).filter(|b| *b != kiln_ci::trigger::ZERO_SHA).map(str::to_owned);
            gitops::trigger(hub, &repo, Trigger { event, branch, sha: after, before, pipeline: None, payload: Some(Arc::new(payload)) }).await?;
            return Ok(());
        }
    }
    // Everything else: the engine's workflows at the commit GitHub's table gives.
    let f = events::facts(hub, &repo).await;
    let refs: HashMap<String, String> = hub.store.refs(repo.id.clone()).await?.into_iter().collect();
    let head = refs.get(&format!("refs/heads/{}", f.default_branch)).cloned().unwrap_or_default();
    let Some(mut e) = github::event(name, &payload, &f.default_branch, &head) else { return Ok(()) };
    if e.sha.is_empty() {
        e.sha = match refs.get(&e.git_ref) {
            Some(s) => s.clone(),
            None => gitops::run_git(hub, &["-C", &f.mirror, "rev-parse", "--verify", "--quiet", &format!("{}^{{commit}}", e.git_ref)], gitops::LS_REMOTE_TIMEOUT)
                .await
                .map(|o| String::from_utf8_lossy(&o).trim().to_owned())
                .unwrap_or_default(),
        };
    }
    if e.sha.len() != 40 {
        tracing::debug!(repo = %repo.name, event = name, "webhook: no commit to run on");
        return Ok(());
    }
    gitops::ensure_commit(hub, &repo, &e.sha).await?;
    let commit = load::load(hub, &repo, &e.sha).await?;
    let (message, author) = gitops::commit_info(hub, &repo, &e.sha).await;
    if e.message.is_none() {
        e.message = Some(message.clone());
    }
    for l in commit.workflows.iter().filter(|l| l.runs) {
        events::start(hub, &repo, &e, l, &message, &author, Value::Null).await?;
    }
    Ok(())
}

/// Records a delivery; whether it is the first time in 24 hours.
async fn first_delivery(hub: &Hub, id: &str, event: &str) -> anyhow::Result<bool> {
    let (id, event) = (id.to_owned(), event.to_owned());
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute("DELETE FROM ci_deliveries WHERE received_at < ?1", params![now - 86_400])?;
            Ok(c.execute("INSERT OR IGNORE INTO ci_deliveries (id, event, received_at) VALUES (?1, ?2, ?3)", params![id, event, now])? == 1)
        })
        .await
}
