//! Continuous restore-and-check (docs/design/backup.md, "Restore-and-check"): the newest second the
//! archive holds whole is restored into a scratch file and checked, its rows against the source's
//! at that second and the file by `PRAGMA integrity_check`, as Meta's continuous restores and
//! GitLab's daily restore tests check theirs ("A backup nobody restores is not a backup",
//! ~/.cache/amphora-t1/research/backup-single-node.md §0.3). The outcome is a typed [`Check`] and
//! a count in [`CheckCounters`] (`amphora_backup_checks_total`).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use amphora_api::blobs::Blobs;
use sha2::{Digest as _, Sha256};

use super::plan::{self, SegmentKey, ThroughKey};
use super::restore::{Restored, restore, through_marks};
use super::ship::list_all;
use super::{SECOND_NS, second_of};
use crate::db::Db;

/// A database's rows, table by table: each table's name, its row count and a SHA-256 over its rows
/// in rowid order. Two databases with the same rows have the same digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowDigest(pub Vec<(String, u64, [u8; 32])>);

/// The digest of every table of `db` but the engine's own (`sqlite_*`).
pub fn digest(db: &Db) -> anyhow::Result<RowDigest> {
    let mut tables = Vec::new();
    db.query(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        [],
        |r| {
            tables.push(r.get::<String>(0).map_err(|e| anyhow::anyhow!("{e}"))?);
            Ok(true)
        },
    )?;
    let mut out = Vec::with_capacity(tables.len());
    for table in tables {
        let quoted = table.replace('"', "\"\"");
        let mut hash = Sha256::new();
        let mut rows = 0u64;
        db.query_owned(
            &format!("SELECT * FROM \"{quoted}\" ORDER BY rowid"),
            [],
            |r| {
                let values: Vec<_> = r.get_values().collect();
                hash.update(format!("{values:?}\n").as_bytes());
                rows = rows.saturating_add(1);
                Ok(true)
            },
        )?;
        out.push((table, rows, hash.finalize().into()));
    }
    Ok(RowDigest(out))
}

/// Why a check failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckFailure {
    /// The archive didn't restore the second.
    Restore(String),
    /// The restored file failed `PRAGMA integrity_check`; its first lines.
    Integrity(String),
    /// The restored rows differ from the source's at that second.
    Rows {
        source: RowDigest,
        restored: RowDigest,
    },
}

/// What one restore-and-check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// The second restored, whole and equal to the source.
    Passed {
        second: u64,
        restored: Restored,
    },
    Failed {
        second: u64,
        why: CheckFailure,
    },
    /// The source could no longer say its rows at the second (it moved on since): nothing compared.
    SourceMoved {
        second: u64,
    },
    /// The archive holds no second whole yet.
    Empty,
}

/// Checks run, by result: what `amphora_backup_checks_total{result}` reports.
#[derive(Debug, Default)]
pub struct CheckCounters {
    pub passed: AtomicU64,
    pub failed: AtomicU64,
    pub source_moved: AtomicU64,
}

/// `amphora_backup_checks_total`: its type and help line, as `CheckCounters` counts it.
pub const CHECK_METRIC: (&str, &str, &str) = (
    "amphora_backup_checks_total",
    "counter",
    "Restore-and-checks of the backup archive, by result (passed, failed, source_moved).",
);

impl CheckCounters {
    fn count(&self, check: &Check) {
        let counter = match check {
            Check::Passed { .. } => &self.passed,
            Check::Failed { .. } => &self.failed,
            Check::SourceMoved { .. } => &self.source_moved,
            Check::Empty => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// The newest second the archive under `prefix` holds whole: the last second its through point
/// covers, none before its first mark.
pub fn newest_second(
    blobs: &dyn Blobs,
    prefix: &str,
    until: Instant,
) -> anyhow::Result<Option<u64>> {
    let mut segments = Vec::new();
    for (key, _) in list_all(blobs, &format!("{prefix}/log/"), until)? {
        segments.push(SegmentKey::parse(prefix, &key)?);
    }
    let marks: Vec<ThroughKey> = through_marks(blobs, prefix, until)?
        .into_iter()
        .map(|(m, _)| m)
        .collect();
    plan::raise_through(&mut segments, &marks);
    let Some(first) = segments
        .iter()
        .find_map(|s| s.marks)
        .map(|(first, _)| first)
    else {
        return Ok(None);
    };
    let through = segments.iter().map(|s| s.through).max().unwrap_or(0);
    // A second `s` is whole once the archive is whole through its end, `s * SECOND_NS`.
    let newest = through / SECOND_NS;
    Ok(if newest >= second_of(first) {
        Some(newest)
    } else {
        None
    })
}

/// Whether a check is due: the archive's cadence sets it, never a timer. One is due when a base
/// landed since the last check (each base is checked once), or the log shipped since the last check
/// outweighs the log that check replayed (the log a restore replays is checked as fast as it grows).
#[must_use]
pub fn check_due(last: Option<(u64, u64)>, newest_base: u64, log_since: u64) -> bool {
    match last {
        None => true,
        Some((base, replayed)) => base != newest_base || log_since >= replayed.max(1),
    }
}

/// Restores the newest whole second under `prefix` into a new file at `scratch` and checks it:
/// `PRAGMA integrity_check`, then its rows against `source(second)` (the source's digest at that
/// second; none when it can no longer say). Counts the outcome. The scratch file is the caller's to
/// remove (it is kept on a failure to look into).
pub fn restore_and_check(
    blobs: &dyn Blobs,
    prefix: &str,
    scratch: &Path,
    source: &dyn Fn(u64) -> anyhow::Result<Option<RowDigest>>,
    counters: &CheckCounters,
    until: Instant,
) -> anyhow::Result<Check> {
    let Some(second) = newest_second(blobs, prefix, until)? else {
        return Ok(Check::Empty);
    };
    restore_and_check_at(blobs, prefix, second, scratch, source, counters, until)
}

/// [`restore_and_check`] at `second`, one the archive holds whole: the shipping thread checks the
/// second its source's quiet snapshot holds ([`super::Marks::quiet_second`]).
pub fn restore_and_check_at(
    blobs: &dyn Blobs,
    prefix: &str,
    second: u64,
    scratch: &Path,
    source: &dyn Fn(u64) -> anyhow::Result<Option<RowDigest>>,
    counters: &CheckCounters,
    until: Instant,
) -> anyhow::Result<Check> {
    let check = check_second(blobs, prefix, second, scratch, source, until)?;
    counters.count(&check);
    match &check {
        Check::Failed { second, why } => tracing::error!(
            target: "amphora.backup.check",
            event = "failed",
            second,
            why = ?why,
        ),
        other => tracing::info!(target: "amphora.backup.check", event = "checked", check = ?other),
    }
    Ok(check)
}

fn check_second(
    blobs: &dyn Blobs,
    prefix: &str,
    second: u64,
    scratch: &Path,
    source: &dyn Fn(u64) -> anyhow::Result<Option<RowDigest>>,
    until: Instant,
) -> anyhow::Result<Check> {
    let restored = match restore(blobs, prefix, second, scratch, until) {
        Ok(restored) => restored,
        Err(e) => {
            return Ok(Check::Failed {
                second,
                why: CheckFailure::Restore(format!("{e:#}")),
            });
        }
    };
    let db = Db::open(scratch)?;
    let mut integrity = Vec::new();
    db.query("PRAGMA integrity_check", [], |r| {
        integrity.push(r.get::<String>(0).map_err(|e| anyhow::anyhow!("{e}"))?);
        Ok(integrity.len() < 8)
    })?;
    if integrity.as_slice() != ["ok"] {
        return Ok(Check::Failed {
            second,
            why: CheckFailure::Integrity(integrity.join("; ")),
        });
    }
    let got = digest(&db)?;
    let Some(want) = source(second)? else {
        return Ok(Check::SourceMoved { second });
    };
    if got != want {
        return Ok(Check::Failed {
            second,
            why: CheckFailure::Rows {
                source: want,
                restored: got,
            },
        });
    }
    Ok(Check::Passed { second, restored })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-T1.12.C2: a check is due at the first, at each new base, and once the log since the last
    /// check outweighs what it replayed; not otherwise.
    #[test]
    fn t_t1_12_c2_check_follows_the_archive_cadence() {
        assert!(check_due(None, 1, 0));
        assert!(check_due(Some((1, 100)), 2, 0));
        assert!(!check_due(Some((1, 100)), 1, 99));
        assert!(check_due(Some((1, 100)), 1, 100));
        assert!(check_due(Some((1, 0)), 1, 1));
        assert!(!check_due(Some((1, 0)), 1, 0));
    }
}
