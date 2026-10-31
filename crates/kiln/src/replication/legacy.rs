//! U15: read-only pre-import fence for the retired Kiln cluster log. This is compatibility
//! inspection, never a live replica/controller. A durable tail requires offline legacy recovery;
//! the importer must not acknowledge a source snapshot that omits it.

use std::path::Path;

use anyhow::Context as _;
use hyper_block::buf::Alignment;
use hyper_log::{Config, Facts};
use super::legacy_read::{LegacyLog, Source};

const LOG_ID: u128 = u128::from_be_bytes(*b"kiln-cluster-gp1");
const GROUP: u128 = 1;

fn settings(align: Alignment) -> anyhow::Result<Config> {
    // U15: retired fixed on-disk settings, retained only to inspect migration input.
    Config::derive(&Facts { align, sealed: false, largest_entry: (8 << 20) + (64 << 10), disk_bytes: 4 << 30, max_groups: 2, cadence_entries: 4096, cadence_bytes: 256 << 20, uncommitted_entries: 2048, uncommitted_bytes: 64 << 20, cache_bytes: 8 << 20 })
        .map_err(|e| anyhow::anyhow!("legacy log settings: {e:?}"))
}

/// Call while the importer's source SQL snapshot is pinned, before creating a Restore. Keep
/// the returned source until final publication and verify it again before completing Restore.
/// Only harmless log recovery writes enter the bounded overlay; the original remains read-only.
pub(crate) fn check(source: &Path, applied: u64, term: u64) -> anyhow::Result<Option<Source>> {
    let parent = source.parent().context("legacy source has no parent")?;
    let path = parent.join("cluster").join("cluster.log");
    let align = Alignment::new(4096).map_err(|e| anyhow::anyhow!("legacy log alignment: {e:?}"))?;
    let Some(log) = LegacyLog::open(&path, settings(align)?, LOG_ID, GROUP)
        .with_context(||format!("U15: {} requires offline legacy recovery before import; source retained",path.display()))? else { return Ok(None); };
    if let Some(view) = log.view() {
        let committed = view.hard_state.map_or(view.start.index, |h| h.commit.max(view.start.index)).max(view.released);
        anyhow::ensure!(!view.proposals.iter().any(|p| p.index > applied), "U15: legacy log retains proposals beyond the SQL source; offline legacy recovery is required before import; source retained at {}", path.display());
        anyhow::ensure!(applied >= committed, "U15: legacy SQL applied index {applied} is behind durable cluster commit {committed}; offline legacy recovery is required before import; source retained at {}", path.display());
        anyhow::ensure!(log.term(applied)? == term,"U15: legacy SQL snapshot term disagrees with its log; source retained at {}",path.display());
    }
    // Shared replay validates the snapshot range, committed suffix continuity and generation.
    // The main SQL importer cannot reconstruct the retired preupdate protocol: any omitted
    // committed entry is a refusal, never a replay into partially imported canonical state.
    let source = log.replay(applied, |_,_,_|anyhow::bail!("U15: legacy SQL source omits a committed log entry; offline legacy recovery is required"))?;
    Ok(Some(source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_block::file::{CachingRequest, DeviceFile};
    use hyper_log::{Entries, Entry, HardState, Log, Update};
    use std::path::PathBuf;
    use std::io::Write as _;

    #[test]
    fn u15_legacy_durable_tail_refuses_import_without_mutating_source() -> anyhow::Result<()> {
        let cache = PathBuf::from(std::env::var_os("HOME").context("HOME")?).join(".cache");
        std::fs::create_dir_all(&cache)?;
        let dir = tempfile::tempdir_in(cache)?;
        let source = dir.path().join("kiln.db");
        let folder = dir.path().join("cluster");
        std::fs::create_dir(&folder)?;
        let path = folder.join("cluster.log");
        let align = Alignment::new(4096)?;
        let device = DeviceFile::open(&path, true, CachingRequest::Buffered, align)?;
        let log = Log::create(device, settings(align)?, LOG_ID).map_err(|e| anyhow::anyhow!("create legacy fixture: {e:?}"))?;
        log.write(GROUP, Update { entries: Some(Entries { first: 1, entries: vec![Entry { term: 1, bytes: vec![1, 2, 3] }] }), hard_state: Some(HardState { term: 1, vote: 1, commit: 1, held: None }), ..Update::default() }).map_err(|e| anyhow::anyhow!("write legacy fixture: {e:?}"))?;
        log.close().map_err(|e| anyhow::anyhow!("close legacy fixture: {e:?}"))?;
        let before = std::fs::read(&path)?;
        let error = check(&source, 0, 0).err().context("a lagging SQL source was accepted")?;
        anyhow::ensure!(error.to_string().contains("behind durable cluster commit"), "wrong refusal: {error:#}");
        assert!(check(&source,1,2).is_err(),"a mismatched SQL snapshot term was accepted");
        assert!(check(&source,2,1).is_err(),"a SQL snapshot ahead of durable commit was accepted");
        let inspection=check(&source,1,1)?.context("fixture log vanished")?;
        inspection.verify()?;
        assert_eq!(std::fs::read(&path)?,before,"harmless recovery must not change source bytes");
        std::fs::OpenOptions::new().append(true).open(&path)?.write_all(b"changed after inspection")?;
        assert!(inspection.verify().is_err(),"a changed log could publish an imported snapshot");
        Ok(())
    }
}
