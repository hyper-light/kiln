//! GitHub's webhooks for the CI engine (docs/ci.md W03-R02, W03-R07, W03-R14). A delivery's
//! payload becomes `github.event` as GitHub sent it.
//!
//! - **Moves polling also sees** (a push, a pull request opened or synchronized): the ref is
//!   recorded atomically with an immutable outbox source, using the compare-and-set polls use, so the move triggers once, whichever
//!   source saw it first. The usual path then runs, `.kiln/` pipelines included.
//! - **Events polling can't see** (comments, reviews, labels, releases, …): they start the
//!   matching workflows of the commit GitHub's table gives.
//! - **Redeliveries:** a delivery id (`X-GitHub-Delivery`) seen in the last 24 hours does nothing.

use std::collections::HashMap;
use std::sync::Arc;

use kiln_ci::trigger::github;
use crate::sql::params;
use serde_json::Value;

use super::{events, schedule::outbox};
use crate::gitops::{self, Trigger};
use crate::hub::Hub;

/// Asks the poller for the repository a GitHub webhook names (its payload's
/// `repository.full_name`), if kiln has it; for any other, the poller is only woken.
pub async fn want_github(hub: &Hub, body: &[u8]) {
    let name = serde_json::from_slice::<Value>(body).ok().and_then(|v| v.pointer("/repository/full_name").and_then(Value::as_str).map(str::to_owned));
    match name {
        Some(n) => gitops::want_named(hub, &n).await,
        None => gitops::wake(hub),
    }
}

/// A delivery, after its signature checked out.
pub async fn github(hub: &Arc<Hub>, name: &str, delivery: Option<String>, body: &[u8]) -> anyhow::Result<()> {
    if !hub.ci.enabled() {
        // Without the engine a webhook only hints that its repository's poll is due.
        want_github(hub, body).await;
        return Ok(());
    }
    // W03-R07: pre-upgrade receipts remain authoritative for their original 24-hour window,
    // even though those historical deliveries have no outbox source. This read never accepts work.
    if let Some(id) = &delivery {
        let id = id.clone();
        let seen = hub.store.call(move |c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_deliveries WHERE id = ?1 AND received_at >= ?2)", params![id, crate::util::now_unix() - 86_400], |r| r.get::<_, bool>(0))?)).await?;
        if seen { return Ok(()); }
    }
    let payload: Value = serde_json::from_slice(body)?;
    let full = payload.pointer("/repository/full_name").and_then(Value::as_str).unwrap_or_default().to_owned();
    let Some(repo) = hub.store.repos().await?.into_iter().find(|r| r.name.eq_ignore_ascii_case(&full)) else {
        // Not one of kiln's: there's nothing of it to poll.
        gitops::wake(hub);
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
            let (event, branch) = if let Some(b) = r.strip_prefix("refs/heads/") {
                ("push", b.to_owned())
            } else if let Some(t) = r.strip_prefix("refs/tags/") {
                ("tag", t.to_owned())
            } else {
                ("pull_request", format!("pr/{}", r.trim_start_matches("refs/pull/").trim_end_matches("/head")))
            };
            let before = payload.pointer("/before").and_then(Value::as_str).filter(|b| *b != kiln_ci::trigger::ZERO_SHA).map(str::to_owned);
            let t = Trigger { event, branch, sha: after.clone(), before, pipeline: None, payload: Some(Arc::new(payload)) };
            let source = events::ref_ingress(hub, &repo, &t).await?;
            if hub.store.advance_ref_ingress(repo.id.clone(), r, after, source, delivery.map(|id| (id, name.to_owned()))).await? {
                outbox::kick(hub).await;
                let u = &hub.cfg.updates;
                if event == "push" && repo.name == u.release_repo && t.branch == u.release_branch { hub.updates.wake_in_step(); }
            }
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
    let source = events::ingress(hub, &repo, e).await?;
    first_delivery(hub, delivery.as_deref(), name, source).await?;
    Ok(())
}

/// W03-R07: delivery deduplication and the complete immutable source are one transaction.
async fn first_delivery(hub: &Hub, id: Option<&str>, event: &str, source: outbox::Payload) -> anyhow::Result<bool> {
    let (id, event) = (id.map(str::to_owned), event.to_owned());
    let fresh = hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        let now = crate::util::now_unix();
        if let Some(id) = &id {
            tx.execute("DELETE FROM ci_deliveries WHERE received_at < ?1", params![now - 86_400])?;
            if tx.execute("INSERT OR IGNORE INTO ci_deliveries (id, event, received_at) VALUES (?1, ?2, ?3)", params![id, event, now])? == 0 { return Ok(false); }
        }
        // A fresh receipt after the 24-hour window is a new source, just as before.
        outbox::insert(&tx, &format!("ingress/{}", crate::util::new_id()), &source)?;
        tx.commit()?;
        Ok(true)
    }).await?;
    if fresh { outbox::kick(hub).await; }
    Ok(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W03-R07/R14: refusal rolls back both ACK markers; a lost wake is recovered from the
    /// immutable source, and replay of its fanout cannot create a second child.
    #[test]
    fn w03_r14_ingress_ref_and_delivery_retry_survive_lost_wake() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let repo = hub.store.upsert_repo(crate::store::Repo { id: "ingress-repo".into(), name: "test/ingress".into(), clone_url: "file:///unused".into(), branches: vec!["main".into()], poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: Some("main".into()), mirror: None }).await?;
            let event = kiln_ci::trigger::Event { name: "push".into(), git_ref: "refs/heads/main".into(), sha: "accepted-sha".into(), payload: serde_json::json!({"immutable": "original"}), ..Default::default() };
            let child = outbox::Workflow { repo: repo.clone(), event: event.clone(), path: ".kiln/check.yaml".into(), workflow: Default::default(), message: "accepted message".into(), author: "accepted author".into(), actions: None, inputs: Value::Null, error: Some("accepted parse error".into()) };
            let source = outbox::Payload::Ingress(Box::new(outbox::Ingress { repo: repo.clone(), event, workflows: vec![child], trunk_before: None }));
            hub.store.call(|c| {
                c.execute("CREATE TRIGGER refuse_ingress BEFORE INSERT ON ci_trigger_outbox BEGIN SELECT RAISE(ABORT, 'injected ingress refusal'); END", [])?;
                Ok(())
            }).await?;
            assert!(first_delivery(&hub, Some("delivery"), "push", source.clone()).await.is_err());
            assert!(hub.store.advance_ref_ingress(repo.id.clone(), "refs/heads/main".into(), "accepted-sha".into(), source.clone(), Some(("move".into(), "push".into()))).await.is_err());
            assert!(hub.store.refs(repo.id.clone()).await?.is_empty());
            let receipts: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM ci_deliveries", [], |r| r.get(0))?)).await?;
            assert_eq!(receipts, 0);
            hub.store.call(|c| { c.execute("DROP TRIGGER refuse_ingress", [])?; Ok(()) }).await?;
            // Legacy receipts deliberately suppress work without fabricating an outbox row.
            hub.store.call(|c| { c.execute("INSERT INTO ci_deliveries (id,event,received_at) VALUES ('legacy','push',?1)", params![crate::util::now_unix()])?; Ok(()) }).await?;
            assert!(!first_delivery(&hub, Some("legacy"), "push", source.clone()).await?);
            let count: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM ci_trigger_outbox", [], |r| r.get(0))?)).await?;
            assert_eq!(count, 0);
            let oversized = outbox::Payload::Rejected { reason: "x".repeat((8 << 20) + 1) };
            assert!(first_delivery(&hub, Some("bounded"), "push", oversized).await.is_err());
            let poisoned: bool = hub.store.call(|c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_deliveries WHERE id = 'bounded')", [], |r| r.get(0))?)).await?;
            assert!(!poisoned);
            assert!(first_delivery(&hub, Some("delivery"), "push", source.clone()).await?);
            let mut changed = source.clone();
            if let outbox::Payload::Ingress(s) = &mut changed { s.event.sha = "later-sha".into(); s.workflows.clear(); }
            assert!(!first_delivery(&hub, Some("delivery"), "push", changed).await?);
            assert!(hub.store.advance_ref_ingress(repo.id.clone(), "refs/heads/main".into(), "accepted-sha".into(), source.clone(), None).await?);
            assert!(!hub.store.advance_ref_ingress(repo.id.clone(), "refs/heads/main".into(), "accepted-sha".into(), source, None).await?);
            // No dispatcher or wake receiver: simulate reopening by reading only committed SQL.
            let sources: Vec<(String, String)> = hub.store.call(|c| Ok(c.prepare("SELECT id,payload FROM ci_trigger_outbox WHERE kind = 'ingress'")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?)).await?;
            assert_eq!(sources.len(), 2);
            for (id, text) in sources {
                let outbox::Payload::Ingress(snapshot) = serde_json::from_str(&text)? else { anyhow::bail!("not an ingress source"); };
                assert_eq!(snapshot.event.sha, "accepted-sha");
                assert_eq!(snapshot.event.payload, serde_json::json!({"immutable":"original"}));
                let children: Vec<_> = snapshot.workflows.into_iter().map(|w| (w.path.clone(), outbox::Payload::Workflow(Box::new(w)))).collect();
                outbox::materialize(&hub, &id, children.clone()).await?;
                outbox::materialize(&hub, &id, children).await?;
            }
            let children: Vec<String> = hub.store.call(|c| Ok(c.prepare("SELECT payload FROM ci_trigger_outbox WHERE kind = 'workflow'")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)).await?;
            assert_eq!(children.len(), 2);
            for text in children {
                let outbox::Payload::Workflow(w) = serde_json::from_str(&text)? else { anyhow::bail!("not a workflow child"); };
                assert_eq!(w.event.sha, "accepted-sha");
                assert_eq!(w.message, "accepted message");
                assert_eq!(w.error.as_deref(), Some("accepted parse error"));
            }
            Ok(())
        })
    }
}
