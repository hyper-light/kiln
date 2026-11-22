//! Restoring several groups to one second (docs/design/backup.md, "One cut across groups"): each
//! group's archive restored to the same second, then any cross-group transaction the cut left
//! half-done finished as a crashed coordinator's are (docs/design/xgroup.md, "Crashes"): a new
//! incarnation of its coordinator scans every group, and each transaction is committed or aborted by
//! its anchor's decision record (or, under Parallel Commits, by whether every intent is there).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use amphora_api::blobs::Blobs;

use super::restore::{Restored, restore};
use crate::database::Database;
use crate::xgroup::{Cluster, Protocol};

/// Restores group `i` from the archive under `groups[i].0` into a new file at `groups[i].1`, every
/// group to the end of `second`, then finishes what coordinator `coordinator` (running `protocol`)
/// left half-done at that cut, by `until`. Group 0 is the coordinator's registry, as
/// [`Cluster::open`] takes them.
pub fn restore_cut(
    blobs: &dyn Blobs,
    groups: &[(String, PathBuf)],
    second: u64,
    coordinator: u64,
    protocol: Protocol,
    until: Instant,
) -> anyhow::Result<Vec<Restored>> {
    let mut restored = Vec::with_capacity(groups.len());
    for (prefix, path) in groups {
        restored.push(restore(blobs, prefix, second, path, until)?);
    }
    let mut databases = Vec::with_capacity(groups.len());
    for (_, path) in groups {
        databases.push(Database::open(path, until)?);
    }
    // Opening the coordinator's next incarnation finishes every transaction earlier ones left
    // before it answers (`Cluster::open`); it runs none of its own, so its route is never asked.
    let cluster = Cluster::open(
        databases,
        Arc::new(|_: &[u8]| 0),
        coordinator,
        protocol,
        1,
        until,
    )?;
    drop(cluster);
    Ok(restored)
}
