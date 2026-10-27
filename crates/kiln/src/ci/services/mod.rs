//! GitHub's runtime services (docs/ci.md W08), for the actions that call them unmodified.
//!
//! - [`cas`]: the content-addressed store under caches and artifacts.
//! - [`blob`]: signed blob URLs, the Azure Blob subset the toolkit uploads and downloads with.
//! - [`cache_v1`]: the cache service as GitHub Enterprise Server speaks it, which is what the
//!   toolkit uses on any server but github.com.
//! - [`twirp`]: the results service: artifacts (v4).
//! - [`pipelines`]: artifacts as v1 to v3 of the artifact actions keep them (file containers).
//! - [`s3`]: S3 credentials, for the OIDC issuer's documents (W10-R12).
//! - [`memo`]: the work ledger for runners (`/_kiln/work/v1/`): steps' reuse (W24-R07).
//!
//! [`maintain`] runs every hour: expired artifacts let go of their content, idle and
//! over-quota cache entries go, then the store drops what nothing refers to.

pub mod blob;
pub mod cache_v1;
pub mod cas;
pub mod chunks;
pub mod memo;
pub mod pipelines;
pub mod s3;
pub mod twirp;

use std::sync::Arc;
use std::time::Duration;

use rusqlite::params;

use crate::hub::Hub;

const EVERY: Duration = Duration::from_secs(3600);

/// The services' upkeep, while the CI engine runs.
pub fn spawn_maintenance(hub: Arc<Hub>) {
    crate::rt::spawn(async move {
        loop {
            crate::rt::sleep(Duration::from_secs(60)).await;
            if let Err(e) = maintain(&hub).await {
                tracing::warn!(error = %format!("{e:#}"), "CI store upkeep failed");
            }
            crate::rt::sleep(EVERY).await;
        }
    });
}

/// One round of upkeep.
pub async fn maintain(hub: &Hub) -> anyhow::Result<()> {
    let now = crate::util::now_unix();
    let (expired, stale) = hub
        .store
        .call(move |c| {
            let expired = c.execute("UPDATE ci_artifacts SET object = NULL WHERE expires_at IS NOT NULL AND expires_at <= ?1 AND object IS NOT NULL", params![now])?;
            // Uploads that never finalized, after a day.
            let stale = c.execute("DELETE FROM ci_artifacts WHERE finalized = 0 AND created_at < ?1", params![now - 86_400])?;
            Ok((expired, stale))
        })
        .await?;
    let evicted = cache_v1::evict(hub, None).await?;
    pipelines::sweep(hub).await;
    crate::ci::token::prune(hub).await?;
    // A store that can't be reached doesn't stop the rest; Settings → OIDC says what went wrong.
    if let Err(e) = crate::ci::oidc::publish(hub).await {
        tracing::warn!(error = %format!("{e:#}"), "OIDC: publishing the issuer's documents failed");
    }
    // The work ledger past its quotas (W24-R15, R21, AC12): each domain keeps the results that save
    // the most time per byte; what the rest kept is collected below.
    if let Some(quotas) = hub.cfg.ci.work.quotas() {
        let gone = hub
            .store
            .call(move |c| {
                let repos: Vec<String> = c.prepare("SELECT name FROM repos")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
                let mut n = 0usize;
                for repo in repos {
                    n = n.saturating_add(crate::ci::work::evict(c, &repo, &quotas)?.len());
                }
                Ok(n)
            })
            .await?;
        if gone > 0 {
            tracing::info!(entries = gone, "work ledger entries evicted under quota, the least time saved per byte first (W24-AC12)");
        }
    }
    // The reaper (W24-R31): the ledger within the data volume's floor and each scope's temporal
    // rules, when free space, the ledger's ceiling or the system's schedule calls for it.
    if let Some((size, free)) = crate::os::disk_space(&hub.cfg.server.data_dir) {
        static LAST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
        let now = crate::util::now_unix();
        let last = Some(LAST.load(std::sync::atomic::Ordering::Relaxed)).filter(|l| *l > 0);
        let went = hub
            .store
            .call(move |c| {
                use kiln_ci::work::reap::{self as r, Scope, Trigger};
                let (system, repos) = crate::ci::reap::stored(c)?;
                let sys = r::resolve(size, &[(Scope::System, &system)]);
                let held: u64 = c.query_row("SELECT COALESCE(SUM(length(value)), 0) FROM work_records", [], |r| r.get::<_, i64>(0)).map(|n| u64::try_from(n).unwrap_or(0))?;
                let Some(trigger) = r::due(&sys, free, held, 0, last, now) else { return Ok((None, Vec::new())) };
                let repos = repos.iter().map(|(name, p)| (name.clone(), r::resolve(size, &[(Scope::System, &system), (Scope::Repo, p)]))).collect();
                let went = crate::ci::work::reap(c, &sys, &repos, free, now)?;
                Ok((Some(trigger == Trigger::Schedule), went))
            })
            .await?;
        if went.0 == Some(true) {
            LAST.store(now, std::sync::atomic::Ordering::Relaxed);
        }
        for e in &went.1 {
            let reason = match e.reason {
                kiln_ci::work::reap::Reason::MaxAge => "max_age",
                kiln_ci::work::reap::Reason::Target => "target",
                kiln_ci::work::reap::Reason::Floor => "floor",
            };
            crate::metrics::global().work_reaped_bytes.add(&["ledger", reason], e.bytes);
            tracing::info!(entry = %e.id, bytes = e.bytes, reason, rule = e.rule, scope = ?e.scope, "the reaper evicted a work ledger entry (W24-R31)");
        }
        if !went.1.is_empty() {
            hub.store.audit("kiln".into(), "ci.work.reap".into(), format!("{} ledger entries, {} bytes", went.1.len(), went.1.iter().map(|e| e.bytes).sum::<u64>())).await;
        }
    }
    // Steps' kept files no result can reuse any more go first, to be collected too (W24-R07).
    memo::sweep(hub).await?;
    let collected = cas::collect(hub).await?;
    if expired + stale + evicted + collected.objects > 0 {
        tracing::info!(expired, stale, evicted, objects = collected.objects, bytes = collected.bytes, "CI store upkeep");
    }
    Ok(())
}
