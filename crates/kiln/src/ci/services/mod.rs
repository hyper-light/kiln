//! GitHub's runtime services (docs/ci.md W08), for the actions that call them unmodified.
//!
//! - [`cas`]: the content-addressed store under caches and artifacts.
//! - [`blob`]: signed blob URLs, the Azure Blob subset the toolkit uploads and downloads with.
//! - [`cache_v1`]: the cache service as GitHub Enterprise Server speaks it, which is what the
//!   toolkit uses on any server but github.com.
//! - [`twirp`]: the results service: artifacts (v4).
//!
//! [`maintain`] runs every hour: expired artifacts let go of their content, idle and
//! over-quota cache entries go, then the store drops what nothing refers to.

pub mod blob;
pub mod cache_v1;
pub mod cas;
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
    crate::ci::token::prune(hub).await?;
    crate::ci::oidc::publish(hub).await?;
    let collected = cas::collect(hub).await?;
    if expired + stale + evicted + collected.objects > 0 {
        tracing::info!(expired, stale, evicted, objects = collected.objects, bytes = collected.bytes, "CI store upkeep");
    }
    Ok(())
}
