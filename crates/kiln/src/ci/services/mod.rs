//! GitHub's runtime services (docs/ci.md W08), for the actions that call them unmodified.
//!
//! - [`cas`]: the content-addressed store under caches and artifacts.
//! - [`blob`]: signed blob URLs, the Azure Blob subset the toolkit uploads and downloads with.
//! - [`cache_v1`]: the cache service as GitHub Enterprise Server speaks it, which is what the
//!   toolkit uses on any server but github.com.
//! - [`twirp`]: the results service: artifacts (v4).
//! - [`remote`]: the store's bucket, with `[ci.store.s3]` (W08-R02).
//!
//! [`maintain`] runs every hour: expired artifacts let go of their content, idle and
//! over-quota cache entries go, then the store drops what nothing refers to. With a bucket, the
//! chunks kept before it was set up go there, and the data directory's cache is trimmed.

pub mod blob;
pub mod cache_v1;
pub mod cas;
pub mod chunks;
pub mod remote;
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
    // A store that can't be reached doesn't stop the rest; Settings → OIDC says what went wrong.
    if let Err(e) = crate::ci::oidc::publish(hub).await {
        tracing::warn!(error = %format!("{e:#}"), "OIDC: publishing the issuer's documents failed");
    }
    let collected = cas::collect(hub).await?;
    let sent = remote::backfill(hub).await?;
    let (trimmed, trimmed_bytes) = remote::trim(hub).await?;
    if expired + stale + evicted + collected.objects + collected.remote + sent + trimmed > 0 {
        tracing::info!(
            expired,
            stale,
            evicted,
            objects = collected.objects,
            chunks = collected.chunks,
            bytes = collected.bytes,
            from_bucket = collected.remote,
            to_bucket = sent,
            trimmed,
            trimmed_bytes,
            "CI store upkeep"
        );
    }
    Ok(())
}
